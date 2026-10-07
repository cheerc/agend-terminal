//! #3694: deadline-bounded pre-removal cleanup of an ignored `target/` build
//! cache.
//!
//! `git worktree remove --force` deletes the whole worktree directory anyway,
//! but a multi-GB `target/` full of build artifacts — often with files still
//! held open by a running cargo/test process — makes that removal slow enough
//! to blow the release budget. Deleting the disposable, git-ignored cache first
//! keeps the subsequent removal quick.
//!
//! The sweep is **deadline-bounded**: if the budget elapses it leaves the
//! remainder in place (the bounded worktree removal that follows deletes it)
//! and logs a warning, rather than consuming the entire release budget or
//! failing the release. A single locked child is skipped, never fatal.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(test)]
mod cleanup_test_seam {
    use std::cell::RefCell;
    use std::path::Path;

    type Hook = Box<dyn Fn(&Path)>;

    thread_local! {
        static AFTER_DIR: RefCell<Option<Hook>> = RefCell::new(None);
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            AFTER_DIR.with(|slot| *slot.borrow_mut() = None);
        }
    }

    pub(crate) fn install(hook: impl Fn(&Path) + 'static) -> Guard {
        AFTER_DIR.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Guard
    }

    pub(super) fn after_dir(path: &Path) {
        AFTER_DIR.with(|slot| {
            if let Some(hook) = slot.borrow().as_ref() {
                hook(path);
            }
        });
    }
}

#[cfg(test)]
use cleanup_test_seam::after_dir;
#[cfg(not(test))]
fn after_dir(_path: &Path) {}

/// #3694: hard cap on time spent pre-deleting an ignored `target/`. Kept well
/// below the 60s `LOCAL_GIT_TIMEOUT` that bounds the subsequent
/// `git worktree remove`, so this sweep can never consume the whole release
/// budget. Normal caches are deleted far inside this window.
pub(crate) const BUILD_CACHE_CLEANUP_BUDGET: Duration = Duration::from_secs(10);

/// Outcome of the pre-removal ignored-cache sweep.
///
/// #40: the sweep's product is a git-ignored build cache, never tracked work.
/// That is what makes a *skip* safe and a *fatal* unnecessary for the "could
/// not delete" cases — but "could not tell what is disposable" is a different
/// failure and must stay fatal, so the two are deliberately distinct variants
/// rather than one skip bucket.
#[derive(Debug)]
pub(crate) enum CacheCleanup {
    /// Every git-ignored cache was removed.
    Complete,
    /// Some caches survived (unreadable directory, or the budget elapsed).
    /// Non-fatal: the release continues and the bounded
    /// `git worktree remove --force` that follows still deletes the whole
    /// worktree directory. The reason is recorded on the release outcome so a
    /// surviving cache is visible instead of silently reported as clean.
    Skipped(String),
    /// Git could not classify what is disposable. Continuing would mean acting
    /// on a directory of unknown provenance, so the caller fails the release
    /// rather than guessing.
    Fatal { path: PathBuf, reason: String },
}

/// Fold a sweep result into a release outcome.
///
/// A skip is recorded and the release continues: the leftover is a git-ignored
/// build cache and the bounded worktree removal still deletes it. A fatal is
/// written onto the outcome — the caller decides whether to return it or carry
/// on, and both sites share this so the two never drift apart.
///
/// Split out of `worktree_pool.rs` for two reasons: the file sits at 2467 of a
/// hard 2500-line ratchet (see `tests/src_file_size_invariant.rs`), and the
/// three release routes were about to hold three hand-copied versions of this
/// decision. The skip-vs-fatal split is the load-bearing part — do not collapse
/// it back into one bucket, and do not inline it.
pub(crate) fn apply_cache_cleanup(
    out: &mut super::ReleaseOutcome,
    result: CacheCleanup,
) -> Result<(), (PathBuf, String)> {
    match result {
        CacheCleanup::Complete => Ok(()),
        CacheCleanup::Skipped(reason) => {
            out.build_cache_cleanup_skipped = Some(reason);
            Ok(())
        }
        CacheCleanup::Fatal { path, reason } => {
            mark_cache_cleanup_fatal(out, &path, &reason);
            Err((path, reason))
        }
    }
}

/// Write the caller-visible failure for a cache the sweep could not classify.
pub(crate) fn mark_cache_cleanup_fatal(out: &mut super::ReleaseOutcome, path: &Path, reason: &str) {
    super::mark_release_incomplete(
        out,
        "build_cache_cleanup",
        path,
        format!(
            "release incomplete: could not classify ignored build cache at {}: {reason}",
            path.display()
        ),
    );
}

/// Remove an ignored build cache before worktree removal, bounded by
/// [`BUILD_CACHE_CLEANUP_BUDGET`].
///
/// #40: returning `Skipped` means the release still succeeds with a recorded
/// reason; `Fatal` means the caller must fail the release. An unreadable
/// `target/` metadata, or a directory that cannot be enumerated at all, is
/// `Fatal` only when it defeats classification — see [`CacheCleanup`].
pub(crate) fn clean_ignored_build_cache(worktree: &Path) -> CacheCleanup {
    clean_ignored_build_cache_with_budget(worktree, BUILD_CACHE_CLEANUP_BUDGET)
}

/// Test seam: [`clean_ignored_build_cache`] with an injectable budget so the
/// bound can be exercised without building a multi-GB fixture.
pub(crate) fn clean_ignored_build_cache_with_budget(
    worktree: &Path,
    budget: Duration,
) -> CacheCleanup {
    // #48: test-only Fatal injection (see `super::cache_fatal_test_seam`).
    // Fires before the deadline starts so the injected outcome is independent
    // of timing.
    #[cfg(test)]
    if let Some((path, reason)) = super::cache_fatal_test_seam::take() {
        return CacheCleanup::Fatal { path, reason };
    }
    // ONE deadline starts before enumeration/classification and is shared by
    // all `check-ignore` calls plus every directory deletion. This is the
    // property most easily broken by a later change: do NOT recompute a
    // per-directory deadline or give every cache its own git timeout. Three
    // 9s caches must still get ONE 10s sweep, not 27s while holding the release
    // locks and then competing with the 60s LOCAL_GIT_TIMEOUT removal.
    let deadline = Instant::now() + budget;

    let dirs = match ignored_cache_dirs(worktree, deadline) {
        Ok(Some(dirs)) => dirs,
        Ok(None) => {
            tracing::warn!(
                path = %worktree.display(),
                budget_secs = budget.as_secs(),
                "release: ignored build-cache discovery exceeded its budget — leaving caches \
                 for worktree removal"
            );
            return CacheCleanup::Skipped(format!(
                "discovery exceeded its {}s budget at {}",
                budget.as_secs(),
                worktree.display()
            ));
        }
        Err((path, reason)) => return CacheCleanup::Fatal { path, reason },
    };

    // #47: per-candidate skips are recorded and the sweep CONTINUES with the
    // remaining candidates, so the outcome never depends on the unsorted
    // `read_dir` order. A skip is one candidate the sweep could not delete;
    // the deadline is different — the shared budget is exhausted, so every
    // remaining candidate would burn the same dead budget and the loop still
    // returns immediately (see the `Deadline` arm below).
    let mut skips: Vec<String> = Vec::new();

    for dir in dirs {
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            // Vanished between enumeration and deletion: someone else already
            // reclaimed it, which is the outcome the sweep wanted anyway.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            // #40: the entry is already known to be git-ignored, so an
            // unreadable stat says only that this sweep cannot delete it. That
            // is not a reason to abort a release the bounded worktree removal
            // would otherwise complete, so it is recorded and skipped.
            // #47: recorded — not returned — so the remaining candidates are
            // still processed whatever order `read_dir` yielded them in.
            Err(error) => {
                tracing::warn!(
                    path = %dir.display(),
                    "release: ignored build cache is unreadable — leaving it for the bounded \
                     worktree removal"
                );
                skips.push(format!(
                    "unreadable ignored cache at {}: {error}",
                    dir.display()
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            // Preserve the old `target` behaviour for a symlink/file while
            // never following it outside the worktree.
            let _ = std::fs::remove_file(&dir);
            continue;
        }
        match remove_dir_all_bounded(&dir, deadline) {
            Ok(()) => after_dir(&dir),
            Err(BoundedRemoval::Deadline) => {
                tracing::warn!(
                    path = %dir.display(),
                    budget_secs = budget.as_secs(),
                    "release: ignored build cache cleanup exceeded its budget — leaving the \
                     remainder for the bounded worktree removal"
                );
                // Every remaining directory would only burn the same exhausted
                // deadline; stop here so the release keeps its single budget.
                return CacheCleanup::Skipped(format!(
                    "cleanup exceeded its {}s budget at {}",
                    budget.as_secs(),
                    dir.display()
                ));
            }
            Err(BoundedRemoval::Io(error)) => {
                tracing::warn!(
                    path = %dir.display(),
                    error = %error,
                    "release: ignored build cache could not be enumerated — leaving it for the \
                     bounded worktree removal"
                );
                // #47: record the skip and continue with the remaining
                // candidates instead of abandoning them.
                skips.push(format!(
                    "could not enumerate ignored cache at {}: {error}",
                    dir.display()
                ));
            }
        }
    }
    if skips.is_empty() {
        CacheCleanup::Complete
    } else {
        CacheCleanup::Skipped(skips.join("; "))
    }
}

#[derive(Debug)]
enum BoundedRemoval {
    /// The budget elapsed before the tree was fully removed; the partial tree
    /// is intentionally left in place.
    Deadline,
    /// A top-level directory could not be read at all.
    Io(String),
}

/// Post-order recursive delete that aborts once `deadline` passes. Individual
/// entry errors are non-fatal (a locked child is skipped) so one open file
/// cannot abort the whole sweep.
fn remove_dir_all_bounded(dir: &Path, deadline: Instant) -> Result<(), BoundedRemoval> {
    if Instant::now() >= deadline {
        return Err(BoundedRemoval::Deadline);
    }
    let entries = std::fs::read_dir(dir).map_err(|e| BoundedRemoval::Io(e.to_string()))?;
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            return Err(BoundedRemoval::Deadline);
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => remove_dir_all_bounded(&path, deadline)?,
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
    let _ = std::fs::remove_dir(dir);
    Ok(())
}

/// #40: enumerate git-ignored top-level directories, so a multi-GB
/// `node_modules/` is swept just like a multi-GB `target/`.
///
/// Git is the authority on "disposable", never a hardcoded list. Every
/// candidate is confirmed by `check-ignore` with the SAME deadline's remaining
/// duration; one expensive ignore check therefore shortens every later check
/// and every deletion instead of multiplying the 60s `LOCAL_GIT_TIMEOUT` by
/// the number of directories.
///
/// Returns `Ok(None)` if the single shared budget is exhausted while enumerating
/// or classifying; the caller records that as a skip. A `git check-ignore`
/// failure is `Fatal`: git is the authority on "disposable", so an unknown
/// verdict must fail the release rather than guess.
fn ignored_cache_dirs(
    worktree: &Path,
    deadline: Instant,
) -> Result<Option<Vec<PathBuf>>, (PathBuf, String)> {
    let entries =
        std::fs::read_dir(worktree).map_err(|error| (worktree.to_path_buf(), error.to_string()))?;
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Structural entries are never disposable. Symlinks are included as
        // candidates only because the sweep unlinks the symlink itself (never
        // follows it); a git-ignored link out of the worktree is still just a
        // link owned by this directory.
        if name == ".git" || name == crate::worktree_pool::MANAGED_MARKER {
            continue;
        }
        let is_dir = file_type.is_dir();
        let is_symlink = file_type.is_symlink();
        let preserve_legacy_target_shape = name == "target" && file_type.is_file();
        if !is_dir && !is_symlink && !preserve_legacy_target_shape {
            continue;
        }
        let pathspec = if is_dir || name == "target" {
            // Preserve the exact pre-#40 `target/` check for the legacy target
            // path, including symlink/file shapes; other ignored caches use
            // their root-level basename.
            format!("{name}/")
        } else {
            name.to_string()
        };
        candidates.push((pathspec, path));
    }
    if candidates.is_empty() {
        return Ok(Some(Vec::new()));
    }

    // One bounded git process per candidate, but every process receives the
    // SAME deadline's remaining duration. Thus N dirs cost at most ONE budget,
    // not N × LOCAL_GIT_TIMEOUT and not N × `budget`.
    let mut ignored = Vec::new();
    for (pathspec, path) in candidates {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let args = ["check-ignore", "-q", "--", pathspec.as_str()];
        match crate::git_helpers::git_bypass_timeout(worktree, &args, remaining) {
            Ok(output) if output.status.success() => ignored.push(path),
            Ok(output) if output.status.code() == Some(1) => {}
            Ok(output) => {
                return Err((
                    path,
                    format!(
                        "git check-ignore failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
            Err(error) => return Err((path, error.to_string())),
        }
    }
    Ok(Some(ignored))
}

#[cfg(test)]
mod tests;
