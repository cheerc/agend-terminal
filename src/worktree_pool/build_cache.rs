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

/// Remove an ignored `target/` cache before worktree removal, bounded by
/// [`BUILD_CACHE_CLEANUP_BUDGET`].
///
/// Returns `Err((path, reason))` only for an opaque precondition failure — an
/// unreadable `target/` metadata or a directory that cannot be enumerated at
/// all (matching the old unbounded `remove_dir_all` contract). Budget
/// exhaustion is a **non-fatal skip**: the leftover is deleted by the worktree
/// removal that follows.
pub(crate) fn clean_ignored_build_cache(worktree: &Path) -> Result<(), (PathBuf, String)> {
    clean_ignored_build_cache_with_budget(worktree, BUILD_CACHE_CLEANUP_BUDGET)
}

/// Test seam: [`clean_ignored_build_cache`] with an injectable budget so the
/// bound can be exercised without building a multi-GB fixture.
pub(crate) fn clean_ignored_build_cache_with_budget(
    worktree: &Path,
    budget: Duration,
) -> Result<(), (PathBuf, String)> {
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
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    for dir in dirs {
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err((dir, error.to_string())),
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
                return Ok(());
            }
            Err(BoundedRemoval::Io(error)) => return Err((dir, error)),
        }
    }
    Ok(())
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
/// or classifying; that is a non-fatal skip, matching the original
/// `target/`-only budget contract. Returns `Err` only for an opaque root or
/// git-classification failure, which the caller surfaces rather than guessing.
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
