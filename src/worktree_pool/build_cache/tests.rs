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

/// #40 regression, upgraded to the #47 contract: an unreadable git-ignored
/// cache directory must NOT fail the release, and must NOT abandon the caches
/// that ARE deletable.
///
/// #45 left this as a degraded single-candidate test that only recorded current
/// behaviour (see the SCOPE note it carried): `clean_ignored_build_cache`
/// returned `Skipped` at the FIRST directory it could not enumerate
/// (`build_cache.rs` `Err(BoundedRemoval::Io)` arm) and the candidate order came
/// from an unsorted `read_dir`, so any assertion about what else got swept
/// encoded a platform's directory order — green on macOS, red on Linux.
///
/// #47 fixes the production defect (record the skip and CONTINUE with the
/// remaining candidates), so this test now pins the real contract with two
/// trapped caches and one deletable cache: whatever order `read_dir` yields,
/// the deletable cache is swept, both trapped caches survive for the bounded
/// worktree removal, and the skip reason names every trapped path. No `sort`
/// is involved — the contract holds for any enumeration order.
///
/// Self-validating, following the established repo pattern: running as root (or
/// on a filesystem that ignores mode bits) the premise cannot be produced, and
/// silently passing would make this a vacuous test.
#[cfg(unix)]
#[test]
fn unreadable_ignored_cache_is_skipped_not_fatal_40() {
    use std::os::unix::fs::PermissionsExt;

    // Two trapped caches, one deletable cache. Whichever order the unsorted
    // `read_dir` yields them in, the deletable one must still be swept.
    const TRAPPED_A: &str = "node_modules";
    const TRAPPED_B: &str = "dist";
    const DELETABLE: &str = "target";

    let wt = bc_fixture("40-unreadable");
    git_repo_ignoring_many(&wt);
    // Deliberately seed nothing else: `git_repo_ignoring_many` lists `.venv`
    // in .gitignore but nothing seeds it, and `read_dir` only yields existing
    // entries — so `.venv` is not a candidate here. A future edit that seeds
    // it must not assume this test covers it.
    seed_dir_with_files(&wt.join(DELETABLE), 4);
    for trapped_cache in [TRAPPED_A, TRAPPED_B] {
        seed_dir_with_files(&wt.join(trapped_cache), 4);
        let trapped = wt.join(trapped_cache).join("locked");
        std::fs::create_dir_all(&trapped).expect("mkdir trapped");
        std::fs::write(trapped.join("content.txt"), b"trapped\n").expect("seed trapped");
        std::fs::set_permissions(&trapped, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    }

    let premise_holds = [TRAPPED_A, TRAPPED_B]
        .iter()
        .all(|cache| std::fs::read_dir(wt.join(cache).join("locked")).is_err());
    if premise_holds {
        let out = clean_ignored_build_cache_with_budget(&wt, Duration::from_secs(30));
        match &out {
            CacheCleanup::Skipped(reason) => {
                // #47: the release outcome reflects per-candidate skips — every
                // trapped path is named.
                for trapped_cache in [TRAPPED_A, TRAPPED_B] {
                    assert!(
                        reason.contains(trapped_cache),
                        "#47: the skip reason must name every trapped path, got: {reason}"
                    );
                }
            }
            other => panic!(
                "#40: an undeletable git-ignored cache is disposable and the worktree removal \
                 that follows still deletes it — the sweep must skip, not abort the release: {other:?}"
            ),
        }

        // #47 order-independence: the trapped caches may come first, last, or
        // around the deletable one — it is swept regardless.
        assert!(
            !wt.join(DELETABLE).exists(),
            "#47: `{DELETABLE}` is deletable and must be swept even though other candidates were trapped"
        );
        for trapped_cache in [TRAPPED_A, TRAPPED_B] {
            assert!(
                wt.join(trapped_cache).exists(),
                "#40: the undeletable cache must survive — the bounded worktree removal \
                 that follows is what deletes it, not this sweep: {out:?}"
            );
        }
    }

    for trapped_cache in [TRAPPED_A, TRAPPED_B] {
        std::fs::set_permissions(
            wt.join(trapped_cache).join("locked"),
            std::fs::Permissions::from_mode(0o755),
        )
        .ok();
    }
    std::fs::remove_dir_all(&wt).ok();
    assert!(
        premise_holds,
        "setup could not produce an unreadable directory (root? permissive fs?) — \
         this machine cannot exercise the guard"
    );
}
