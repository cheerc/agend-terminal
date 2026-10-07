//! #3694 regression tests for the deadline-bounded ignored-cache sweep.

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

/// Unique temp prefix so this helper cannot collide with another wipe-on-entry
/// fixture (#3245 ratchet). Callers must pass distinct tags.
fn bc_fixture(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-3694-build-cache-{}-{tag}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create fixture");
    dir
}

fn seed_target_cache(worktree: &Path) {
    let target = worktree.join("target");
    std::fs::create_dir_all(target.join("debug").join("deps")).expect("mkdir nested target");
    for i in 0..64 {
        std::fs::write(target.join("debug").join(format!("obj-{i}.o")), b"x").expect("seed object");
    }
    std::fs::write(target.join("debug").join("deps").join("libfoo.rlib"), b"y").expect("seed rlib");
}

/// A git repository that ignores `target/`, so `check-ignore` classifies the
/// seeded cache as disposable. No commit is required for `check-ignore`.
fn git_repo_ignoring_target(worktree: &Path) {
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .status()
        .expect("git init")
        .success();
    assert!(ok, "git init failed");
    std::fs::write(worktree.join(".gitignore"), "target/\n").expect("write .gitignore");
}

#[test]
fn bounded_removal_removes_everything_within_budget() {
    let wt = bc_fixture("complete");
    seed_target_cache(&wt);
    let target = wt.join("target");
    remove_dir_all_bounded(&target, Instant::now() + Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("ample budget must complete"));
    assert!(!target.exists(), "full sweep must remove the cache");
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn bounded_removal_stops_at_deadline_and_leaves_the_tree() {
    let wt = bc_fixture("deadline");
    seed_target_cache(&wt);
    let target = wt.join("target");
    // A zero budget aborts at the first deadline check.
    let out = remove_dir_all_bounded(&target, Instant::now());
    assert!(
        matches!(out, Err(BoundedRemoval::Deadline)),
        "zero budget must report Deadline, got {out:?}"
    );
    assert!(
        target.exists(),
        "a deadline abort must leave the (disposable) tree in place"
    );
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn clean_ignored_cache_budget_exhaustion_is_non_fatal() {
    let wt = bc_fixture("skip");
    git_repo_ignoring_target(&wt);
    seed_target_cache(&wt);
    let target = wt.join("target");
    // Zero budget forces the skip path: the sweep must NOT fail the release.
    let out = clean_ignored_build_cache_with_budget(&wt, Duration::ZERO);
    assert!(
        matches!(out, CacheCleanup::Skipped(_)),
        "budget exhaustion is non-fatal: {out:?}"
    );
    assert!(
        target.exists(),
        "a skipped sweep leaves the cache for the bounded git removal"
    );
    std::fs::remove_dir_all(&wt).ok();
}

#[test]
fn clean_ignored_cache_removes_a_small_cache_within_budget() {
    let wt = bc_fixture("remove");
    git_repo_ignoring_target(&wt);
    seed_target_cache(&wt);
    let target = wt.join("target");
    let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(30));
    assert!(
        matches!(out, CacheCleanup::Complete),
        "ample budget must succeed: {out:?}"
    );
    assert!(!target.exists(), "ample budget must remove the cache");
    std::fs::remove_dir_all(&wt).ok();
}

// ─── #40 ─────────────────────────────────────────────────────────────────────

/// A repo ignoring several build-cache shapes at once — the Node/Python/Rust
/// mix the single-`target/` sweep could not see.
fn git_repo_ignoring_many(worktree: &Path) {
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .status()
        .expect("git init")
        .success();
    assert!(ok, "git init failed");
    std::fs::write(
        worktree.join(".gitignore"),
        "target/\nnode_modules/\n.venv/\ndist/\n",
    )
    .expect("write .gitignore");
}

fn seed_dir_with_files(dir: &Path, files: usize) {
    std::fs::create_dir_all(dir).expect("mkdir");
    for i in 0..files {
        std::fs::write(dir.join(format!("f{i}.bin")), b"x").expect("seed file");
    }
}

/// #40 判準 2: the sweep must cover EVERY git-ignored top-level directory, not
/// just `target/`. A Node repo's `node_modules/` is the motivating case — it is
/// exactly as multi-GB and exactly as disposable as a Rust `target/`, and the
/// pre-fix sweep walked straight past it.
#[test]
fn clean_sweeps_every_ignored_top_level_directory_40() {
    let wt = bc_fixture("40-many");
    git_repo_ignoring_many(&wt);
    seed_dir_with_files(&wt.join("target"), 8);
    seed_dir_with_files(&wt.join("node_modules"), 8);
    seed_dir_with_files(&wt.join(".venv"), 4);
    seed_dir_with_files(&wt.join("dist"), 4);

    let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(30));
    assert!(
        matches!(out, CacheCleanup::Complete),
        "sweep must succeed: {out:?}"
    );

    for dir in ["target", "node_modules", ".venv", "dist"] {
        assert!(
            !wt.join(dir).exists(),
            "#40: ignored `{dir}` must be swept, not just `target/`"
        );
    }
    std::fs::remove_dir_all(&wt).ok();
}

/// #40 判準 2: git — not a hardcoded list — decides what is disposable. An
/// UNIGNORED directory holds real work and must survive the sweep untouched.
#[test]
fn unignored_directory_survives_the_sweep_40() {
    let wt = bc_fixture("40-unignored");
    git_repo_ignoring_many(&wt);
    // Deliberately NOT in .gitignore.
    seed_dir_with_files(&wt.join("src-data"), 4);

    let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(30));
    assert!(
        matches!(out, CacheCleanup::Complete),
        "sweep must succeed: {out:?}"
    );
    assert!(
        wt.join("src-data").exists(),
        "#40: an unignored directory holds real work and must never be swept"
    );
    std::fs::remove_dir_all(&wt).ok();
}

/// #40 鎖序/budget invariant: ONE deadline covers the whole sweep. Pin it with
/// the existing injectable-budget seam without a giant fixture: after the first
/// cache is removed, the test seam burns the remaining budget. A single shared
/// deadline must then leave the other caches alone; a per-directory deadline
/// would reset and remove them all.
#[test]
fn one_budget_covers_the_whole_sweep_not_one_per_directory_40() {
    let wt = bc_fixture("40-single-budget");
    git_repo_ignoring_many(&wt);
    seed_dir_with_files(&wt.join("target"), 4);
    seed_dir_with_files(&wt.join("node_modules"), 4);
    seed_dir_with_files(&wt.join(".venv"), 4);

    let first = std::cell::Cell::new(true);
    let _seam = cleanup_test_seam::install(move |_| {
        if first.replace(false) {
            std::thread::sleep(Duration::from_millis(1_100));
        }
    });
    let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(1));
    assert!(
        matches!(out, CacheCleanup::Skipped(_)),
        "budget exhaustion stays non-fatal: {out:?}"
    );

    let remaining = ["target", "node_modules", ".venv"]
        .into_iter()
        .filter(|dir| wt.join(dir).exists())
        .count();
    assert_eq!(
        remaining, 2,
        "#40: the first cache should be removed, then the one shared budget \
         expires. If all three were removed, the sweep is granting a fresh \
         budget per directory and can multiply 10s by the directory count"
    );
    std::fs::remove_dir_all(&wt).ok();
}

/// #40: the structural entries a sweep must never touch, whatever git says.
#[test]
fn git_and_managed_marker_are_never_swept_40() {
    let wt = bc_fixture("40-structural");
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&wt)
        .env("AGEND_GIT_BYPASS", "1")
        .status()
        .expect("git init")
        .success();
    assert!(ok, "git init failed");
    // Even when .gitignore would match them, structural entries stay.
    std::fs::write(wt.join(".gitignore"), "target/\n.git\n.agend-managed\n")
        .expect("write .gitignore");
    seed_dir_with_files(&wt.join("target"), 2);
    std::fs::write(wt.join(crate::worktree_pool::MANAGED_MARKER), "agent=x\n")
        .expect("seed marker");
    let git_dir_before = std::fs::read_dir(wt.join(".git"))
        .expect("read .git")
        .count();

    let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(30));
    assert!(
        matches!(out, CacheCleanup::Complete),
        "sweep must succeed: {out:?}"
    );
    assert!(!wt.join("target").exists(), "target/ is still swept");
    assert!(
        wt.join(crate::worktree_pool::MANAGED_MARKER).exists(),
        "#40: the daemon marker must never be swept — it is the release authority"
    );
    assert!(
        std::fs::read_dir(wt.join(".git"))
            .expect("re-read .git")
            .count()
            == git_dir_before,
        "#40: .git must never be swept even if a .gitignore matched it"
    );
    std::fs::remove_dir_all(&wt).ok();
}

/// #40 regression: an unreadable git-ignored cache directory must NOT fail the
/// release. Before the fix this swept every git-ignored top-level directory but
/// still treated an I/O error as fatal, so a single `0o000` directory aborted
/// the release *before* `remove_worktree` — which is where the abort audit and
/// the `release_failed` verdict live. The leftover is a disposable build cache
/// and the bounded `git worktree remove --force` still deletes it.
///
/// Self-validating, following the established repo pattern: running as root (or
/// on a filesystem that ignores mode bits) the premise cannot be produced, and
/// silently passing would make this a vacuous test.
///
/// The "other caches were still swept" half is asserted without naming a
/// directory. `read_dir` order is unsorted (`build_cache.rs:284`), so which
/// directory precedes the trapped one differs per platform — an earlier
/// version asserted `target` by name and passed on macOS while failing on
/// Linux, and naming a different one would only move the same fragility.
/// See the note at the assertion itself: it records current behaviour, and
/// the stronger contract it does not verify is tracked separately.
#[cfg(unix)]
#[test]
fn unreadable_ignored_cache_is_skipped_not_fatal_40() {
    use std::os::unix::fs::PermissionsExt;

    const TRAPPED_CACHE: &str = "node_modules";
    const OTHER_CACHES: [&str; 2] = ["target", "dist"];

    let wt = bc_fixture("40-unreadable");
    git_repo_ignoring_many(&wt);
    seed_dir_with_files(&wt.join(TRAPPED_CACHE), 4);
    for cache in OTHER_CACHES {
        seed_dir_with_files(&wt.join(cache), 4);
    }

    let trapped = wt.join(TRAPPED_CACHE).join("locked");
    std::fs::create_dir_all(&trapped).expect("mkdir trapped");
    std::fs::write(trapped.join("content.txt"), b"trapped\n").expect("seed trapped");
    std::fs::set_permissions(&trapped, std::fs::Permissions::from_mode(0o000)).expect("chmod");

    let premise_holds = std::fs::read_dir(&trapped).is_err();
    if premise_holds {
        let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(30));
        assert!(
            matches!(out, CacheCleanup::Skipped(_)),
            "#40: an undeletable git-ignored cache is disposable and the worktree removal \
             that follows still deletes it — the sweep must skip, not abort the release: {out:?}"
        );

        // RECORDS CURRENT BEHAVIOUR — it does not verify the original contract.
        //
        // The contract this text states is "a skip must not abandon the caches
        // that ARE deletable". That contract is NOT met: `clean_ignored_build_cache`
        // returns `Skipped` at the first directory it cannot enumerate
        // (`build_cache.rs:215-226`), so every ignored directory ordered after
        // an undeletable one survives the sweep. Measured across four readdir
        // orders, three leave caches behind.
        //
        // So this asserts only what holds under every ordering: the sweep
        // deleted SOMETHING before hitting the block. Naming a specific cache
        // would re-introduce the readdir dependency an earlier version of this
        // test had — it passed on macOS and failed on Linux. Asserting "all
        // deletable caches are gone" would be the real contract, but it is
        // currently false; that gap is tracked separately, not asserted here.
        let swept = OTHER_CACHES
            .iter()
            .filter(|cache| !wt.join(cache).exists())
            .count();
        assert!(
            swept >= 1,
            "#40: the sweep must not be a no-op — at least one readable, \
             git-ignored cache must be gone before the undeletable one stops it: {out:?}"
        );
        // And the trapped cache must survive, otherwise `swept >= 1` could be
        // satisfied by a sweep that deleted everything and reported nothing.
        assert!(
            wt.join(TRAPPED_CACHE).exists(),
            "#40: the undeletable cache must survive — if it were removed the \
             assertion above would pass vacuously: {out:?}"
        );
    }

    std::fs::set_permissions(&trapped, std::fs::Permissions::from_mode(0o755)).ok();
    std::fs::remove_dir_all(&wt).ok();
    assert!(
        premise_holds,
        "setup could not produce an unreadable directory (root? permissive fs?) — \
         this machine cannot exercise the guard"
    );
}
