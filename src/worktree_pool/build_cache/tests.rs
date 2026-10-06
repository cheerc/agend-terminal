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
    assert!(out.is_ok(), "budget exhaustion is non-fatal: {out:?}");
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
    assert!(out.is_ok(), "ample budget must succeed: {out:?}");
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
    assert!(out.is_ok(), "sweep must succeed: {out:?}");

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
    assert!(out.is_ok(), "sweep must succeed: {out:?}");
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
    assert!(out.is_ok(), "budget exhaustion stays non-fatal: {out:?}");

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
    assert!(out.is_ok(), "sweep must succeed: {out:?}");
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
