//! Removal-stage outcomes for `worktree_pool`'s release transaction.
//!
//! Kept separate from the main lifecycle state machine so the core release
//! module stays under the source-file LOC ratchet. #40 adds a distinct
//! `PartiallyRemoved` state: unlike an intact-but-failed target, tracked files
//! may already have been unlinked before git's local-operation timeout killed
//! the removal process.

use super::ReleaseOutcome;
use std::path::Path;

/// Result of worktree-directory removal. These are deliberately distinct:
/// `Failed` means removal did not complete but did not report a timed-out
/// mid-walk; `PartiallyRemoved` means the target survives a timeout/fallback
/// and may have tracked files missing. Callers must not treat the latter as an
/// intact worktree.
pub(super) enum WorktreeRemoval {
    Removed,
    AlreadyAbsent,
    Unmanaged(String),
    PartiallyRemoved { cause: String },
    Failed(String),
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

pub(super) fn remove_worktree(agent: &str, wt_path: &Path, source_repo: &Path) -> WorktreeRemoval {
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
                WorktreeRemoval::PartiallyRemoved {
                    cause: format!(
                        "git worktree remove failed and the directory survived the \
                         remove_dir_all fallback (tracked files may already be \
                         deleted): {stderr}"
                    ),
                }
            }
        }
        Err(error) => {
            if error.kind() == std::io::ErrorKind::TimedOut
                && std::fs::symlink_metadata(wt_path).is_ok()
            {
                tracing::warn!(agent, error = %error, path = %wt_path.display(),
                    "release: worktree removal timed out mid-walk — directory \
                     survives with tracked files possibly already deleted");
                return WorktreeRemoval::PartiallyRemoved {
                    cause: format!("git command failed: {error}"),
                };
            }
            tracing::warn!(agent, error = %error, "git command failed for release");
            WorktreeRemoval::Failed(format!("git command failed: {error}"))
        }
    }
}
