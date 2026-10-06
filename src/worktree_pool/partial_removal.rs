//! Removal-stage outcomes for `worktree_pool`'s release transaction.
//!
//! Kept separate from the main lifecycle state machine so the core release
//! module stays under the source-file LOC ratchet. #40 adds a distinct
//! `PartiallyRemoved` state: unlike an intact-but-failed target, tracked files
//! may already have been unlinked before git's local-operation timeout killed
//! the removal process.

use super::ReleaseOutcome;
use std::collections::BTreeSet;
use std::path::Path;

/// Result of worktree-directory removal. These are deliberately distinct:
/// `Failed` means removal did not complete AND every tracked file is still
/// present; `PartiallyRemoved` means tracked files actually disappeared, so the
/// target must never be treated as an intact worktree.
///
/// #40: the distinction is decided by comparing the tracked path SET across the
/// removal attempt, never by asking whether the directory survived. A
/// permission-blocked directory also survives while every tracked file is
/// intact, and labelling that "possibly deleted" broadcasts a damaged state
/// that does not exist.
#[derive(Debug)]
pub(super) enum WorktreeRemoval {
    Removed,
    AlreadyAbsent,
    Unmanaged(String),
    PartiallyRemoved {
        cause: String,
        /// Tracked paths present before the attempt and gone now. An
        /// orchestrator judges the release from this, so it is carried rather
        /// than reduced to a count.
        missing_tracked: Vec<String>,
    },
    Failed(String),
}

/// Snapshot the tracked path set, or `None` when it cannot be enumerated.
///
/// Taken at the ENTRY of the whole removal attempt, before
/// `git worktree remove` and before the `remove_dir_all` fallback: both can
/// unlink tracked files, so a baseline captured afterwards would already have
/// missed them and a genuine partial removal would read as a clean failure.
pub(super) fn tracked_path_snapshot(worktree: &Path) -> Option<BTreeSet<String>> {
    let output = crate::git_helpers::git_bypass(worktree, &["ls-files", "-z"]).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Tracked paths present in `before` that are no longer on disk.
///
/// Existence is checked against the FILESYSTEM, never by re-reading
/// `git ls-files`: that command reads the INDEX, and unlinking a working-tree
/// file does not remove its index entry — so an index-based comparison reports
/// every file as still present and can never detect a partial removal at all.
/// The index is what makes `before` a trustworthy fixed reference, precisely
/// because deletion does not perturb it.
///
/// Compared as SETS, not counts: untracked noise cannot perturb the verdict,
/// and the result names exactly which tracked files are gone.
fn missing_tracked_paths(worktree: &Path, before: &BTreeSet<String>) -> Vec<String> {
    before
        .iter()
        .filter(|relative| !worktree.join(relative).exists())
        .cloned()
        .collect()
}

/// #40: the shared response to a genuinely damaged remnant.
///
/// Every release route must do the same two things when tracked files really
/// are gone: fail the release with an error naming which paths are missing, and
/// publish the durable unusable state so neither the agent nor the
/// orchestrator keeps treating the tree as usable. Routing all three call sites
/// through one function keeps them from drifting apart.
///
/// Split out of `worktree_pool.rs` for two reasons: the file is at 2467 of a
/// hard 2500-line ratchet (see `tests/src_file_size_invariant.rs`), and three
/// hand-copied copies of this sequence is exactly how they would drift. Do not
/// inline it back.
pub(super) fn record_damaged_remnant(
    out: &mut ReleaseOutcome,
    stage: &'static str,
    target: &Path,
    home: &Path,
    agent: &str,
    cause: &str,
    missing_tracked: &[String],
) {
    mark_release_incomplete(
        out,
        stage,
        target,
        partially_removed_cause(cause, missing_tracked),
    );
    record_worktree_unusable(out, home, agent, target);
}

/// #40: render a partial removal for the caller-visible error, naming the
/// tracked paths that are actually gone rather than only counting them.
///
/// A long list is truncated so an incident cannot inflate the release error
/// without bound, but the count and a representative sample always survive —
/// the orchestrator needs to know how much is missing even when the full set
/// does not fit.
pub(super) fn partially_removed_cause(cause: &str, missing_tracked: &[String]) -> String {
    const SAMPLE: usize = 10;
    if missing_tracked.is_empty() {
        return cause.to_string();
    }
    let sample = missing_tracked
        .iter()
        .take(SAMPLE)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if missing_tracked.len() > SAMPLE {
        format!(
            "{cause}: {} tracked files are missing (e.g. {sample})",
            missing_tracked.len()
        )
    } else {
        format!(
            "{cause}: {} tracked file(s) missing: {sample}",
            missing_tracked.len()
        )
    }
}

/// #40: classify a removal attempt that left the directory on disk.
///
/// `before` is the pre-attempt tracked snapshot. `None` means it could not be
/// taken, and the verdict then stays conservatively "possibly damaged" —
/// over-warning about an intact worktree is recoverable, silently calling a
/// truncated one intact is not.
fn classify_surviving_remnant(
    wt_path: &Path,
    before: Option<&BTreeSet<String>>,
    reason: &str,
) -> WorktreeRemoval {
    match before.map(|before| missing_tracked_paths(wt_path, before)) {
        // Every tracked file survives: nothing was deleted, the removal was
        // simply refused (permissions, a held handle, ...).
        Some(missing) if missing.is_empty() => WorktreeRemoval::Failed(reason.to_string()),
        Some(missing) => WorktreeRemoval::PartiallyRemoved {
            cause: reason.to_string(),
            missing_tracked: missing,
        },
        None => WorktreeRemoval::PartiallyRemoved {
            cause: format!(
                "{reason} (tracked-file set could not be compared, so damage cannot be \
                 ruled out)"
            ),
            missing_tracked: Vec::new(),
        },
    }
}

/// #40: record the damaged directory in the durable deletion-recovery tombstone
/// so `binding_state` can surface it to both agent and orchestrator.
pub(super) fn mark_worktree_unusable(
    home: &Path,
    agent: &str,
    target: &Path,
) -> Result<(), String> {
    let cause = format!(
        "worktree removal did not complete; {} survives in an unknown state \
         (tracked files may already be deleted)",
        target.display()
    );
    crate::agent::deletion_recovery::mark_worktree_unusable(home, agent, &cause)
}

/// If the explicit unusable marker cannot be persisted, surface that fact in
/// the same caller-visible release error; do not leave it only in a log.
pub(super) fn record_worktree_unusable(
    out: &mut ReleaseOutcome,
    home: &Path,
    agent: &str,
    target: &Path,
) {
    if let Err(marker_error) = mark_worktree_unusable(home, agent, target) {
        if let Some(error) = out.error.as_mut() {
            error.push_str(&format!(
                "; additionally, unusable-state marker failed: {marker_error}"
            ));
        } else {
            out.error = Some(format!("unusable-state marker failed: {marker_error}"));
        }
    }
}

pub(super) fn remaining_bytes(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| remaining_bytes(&entry.path()))
                .fold(0_u64, u64::saturating_add)
        })
        .unwrap_or(0)
}

pub(super) fn mark_release_incomplete(
    out: &mut ReleaseOutcome,
    stage: &'static str,
    path: &Path,
    error: String,
) {
    out.error = Some(error);
    out.code = Some("release_incomplete");
    out.stage = Some(stage);
    out.path = Some(path.display().to_string());
    out.bytes_remaining = Some(remaining_bytes(path));
}

/// `tracked_before` is the caller's tracked-path baseline captured before any
/// deletion the release performs (the ignored-cache sweep included). Pass `None`
/// to let this function snapshot on entry — correct only for callers with no
/// earlier deletion step.
pub(super) fn remove_worktree(
    agent: &str,
    wt_path: &Path,
    source_repo: &Path,
    tracked_before: Option<&BTreeSet<String>>,
) -> WorktreeRemoval {
    let owned_baseline;
    let tracked_before: Option<&BTreeSet<String>> = match tracked_before {
        Some(baseline) => Some(baseline),
        None => {
            owned_baseline = tracked_path_snapshot(wt_path);
            owned_baseline.as_ref()
        }
    };
    match std::fs::symlink_metadata(wt_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(agent, path = %wt_path.display(),
                "release: worktree path already absent — pruning registry + clearing binding");
            if !source_repo.as_os_str().is_empty() {
                let _ = crate::git_helpers::git_bypass(source_repo, &["worktree", "prune"]);
            }
            return WorktreeRemoval::AlreadyAbsent;
        }
        Err(e) => {
            return WorktreeRemoval::Failed(format!(
                "opaque worktree target metadata at {}: {e}",
                wt_path.display()
            ))
        }
        Ok(meta) if !meta.is_dir() => {
            return WorktreeRemoval::Failed(format!(
                "opaque worktree target metadata at {}",
                wt_path.display()
            ))
        }
        Ok(_) => {}
    }
    if !super::is_daemon_managed(wt_path) {
        tracing::warn!(agent, path = %wt_path.display(),
            "release skipped: no .agend-managed marker — worktree left alone");
        return WorktreeRemoval::Unmanaged(format!(
            "worktree at {} has no .agend-managed marker — refusing to remove (binding NOT cleared)",
            wt_path.display()
        ));
    }

    // #2550 W2: empty source_repo → `git_worktree::remove_force` runs with NO
    // `current_dir` (git resolves the repo from `--force <abs wt>` itself;
    // `git_cmd`/`git_bypass` both REQUIRE a cwd, and `wt_path.parent()` is
    // wrong — it's the worktrees-pool dir, outside the repo tree, per lead
    // ruling). Converged with `worktree_pool/workspace.rs::teardown_workspace_worktree`'s
    // byte-identical dual-cwd arm (see git_worktree.rs module doc).
    // TODO(W1.2): audit whether the empty-source_repo branch is still
    // reachable in practice; if dead, delete this arm rather than migrate it.
    let wt_str = wt_path.display().to_string();
    #[cfg(test)]
    let injected_error = super::release_test_seam::take_remove_error();
    #[cfg(test)]
    let result = if let Some(kind) = injected_error {
        Err(std::io::Error::new(kind, "#40 injected remove failure"))
    } else {
        crate::git_worktree::remove_force(source_repo, &wt_str)
    };
    #[cfg(not(test))]
    let result = crate::git_worktree::remove_force(source_repo, &wt_str);
    match result {
        Ok(output) if output.status.success() => WorktreeRemoval::Removed,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            tracing::warn!(agent, error = %stderr, path = %wt_path.display(),
                "git worktree remove failed — falling back to remove_dir_all");
            let _ = std::fs::remove_dir_all(wt_path);
            if matches!(
                std::fs::symlink_metadata(wt_path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            ) {
                if !source_repo.as_os_str().is_empty() {
                    if let Err(e) =
                        crate::git_helpers::git_bypass(source_repo, &["worktree", "prune"])
                    {
                        tracing::warn!(agent, error = %e, "git worktree prune failed");
                    }
                }
                WorktreeRemoval::Removed
            } else {
                classify_surviving_remnant(
                    wt_path,
                    tracked_before,
                    &format!(
                        "git worktree remove failed and the directory survived the \
                         remove_dir_all fallback: {stderr}"
                    ),
                )
            }
        }
        Err(error) => {
            if error.kind() == std::io::ErrorKind::TimedOut
                && std::fs::symlink_metadata(wt_path).is_ok()
            {
                tracing::warn!(agent, error = %error, path = %wt_path.display(),
                    "release: worktree removal timed out mid-walk — checking which \
                     tracked files, if any, are already gone");
                return classify_surviving_remnant(
                    wt_path,
                    tracked_before,
                    &format!("git command failed: {error}"),
                );
            }
            tracing::warn!(agent, error = %error, "git command failed for release");
            WorktreeRemoval::Failed(format!("git command failed: {error}"))
        }
    }
}
