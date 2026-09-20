//! Operator-only recovery for a bound review worktree whose marker and Git
//! pointer were lost during a timed-out release.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryReport {
    pub archive: PathBuf,
}

/// Recover one exact markerless bound worktree.
///
/// This RED implementation is intentionally fail-closed until the archive and
/// binding transaction is implemented. Ordinary agent release must continue to
/// refuse markerless targets.
#[allow(clippy::too_many_arguments)]
pub(crate) fn recover_markerless_bound_worktree(
    _home: &Path,
    _actor: &str,
    _audit_reason: &str,
    _instance: &str,
    _branch: &str,
    _worktree: &Path,
    _source_repo: &Path,
) -> Result<RecoveryReport, String> {
    Err("markerless worktree recovery is not implemented".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-worktree-recovery-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn markerless_bound_worktree_is_archived_and_binding_cleared() {
        let home = temp_home("happy");
        let instance = format!("recovery-test-{}", std::process::id());
        let branch = "review/orphan";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-orphan");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("leftover.txt"), b"preserve me").unwrap();
        crate::binding::bind_full(
            &home,
            &instance,
            "",
            branch,
            &worktree,
            &source_repo,
            false,
        )
        .unwrap();

        let report = recover_markerless_bound_worktree(
            &home,
            "operator",
            "recover timed-out review worktree",
            &instance,
            branch,
            &worktree,
            &source_repo,
        )
        .expect("known markerless binding should be recoverable");

        assert!(!worktree.exists());
        assert_eq!(std::fs::read(report.archive.join("leftover.txt")).unwrap(), b"preserve me");
        assert!(crate::binding::read(&home, &instance).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}
