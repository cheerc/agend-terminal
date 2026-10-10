//! Removal-stage outcomes for `worktree_pool`'s release transaction.
//!
//! Kept separate from the main lifecycle state machine so the core release
//! module stays under the source-file LOC ratchet. #40 adds a distinct
//! `PartiallyRemoved` state: unlike an intact-but-failed target, tracked files
//! may already have been unlinked before git's local-operation timeout killed
//! the removal process.

use super::ReleaseOutcome;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// #40: the deadline for the RELEASE removal's `git worktree remove
/// --force`, deliberately longer than [`crate::git_helpers::LOCAL_GIT_TIMEOUT`].
///
/// A release is the one removal whose target is routinely large: a full
/// checkout plus its untracked build artifacts, so git's own walk legitimately
/// outruns the 60s bound sized for ordinary local ops. Killing it at 60s left
/// the target half-unlinked — the failure this constant exists to prevent.
///
/// It is scoped to this path on purpose. Every other `remove_force` caller
/// (workspace teardown, GC) keeps the global 60s default: a longer deadline
/// there would only delay detecting a genuinely wedged removal. The release
/// proxy already reports `release_in_flight` at 45s and the daemon notifies
/// completion via `system:release_completed`, so a slow release is visible to
/// the caller rather than silent.
pub(super) const RELEASE_GIT_TIMEOUT: Duration = Duration::from_secs(600);

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
///
/// Existence is `symlink_metadata`, NOT `Path::exists()`. The criterion is
/// "is this path still on disk", not "does its target resolve": `Path::exists()`
/// calls `stat(2)`, which FOLLOWS a symlink, so a committed link whose target
/// never existed reads as missing even though nothing was deleted — republishing
/// a healthy worktree as damaged. `symlink_metadata` is `lstat(2)`, which answers
/// the question actually being asked.
///
/// It also handles the parent-directory case correctly. A tracked path under a
/// directory that was itself deleted yields `Err` (the file is genuinely
/// unreachable → correctly reported missing), and a path reached THROUGH a
/// symlinked parent directory resolves normally, failing only if a real
/// component is gone. Those two plus the two symlink shapes exhaust the cases:
/// there is no third way for `lstat` to disagree with "the entry itself is gone".
fn missing_tracked_paths(worktree: &Path, before: &BTreeSet<String>) -> Vec<String> {
    before
        .iter()
        .filter(|relative| std::fs::symlink_metadata(worktree.join(relative)).is_err())
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
        crate::git_worktree::remove_force_timeout(source_repo, &wt_str, RELEASE_GIT_TIMEOUT)
    };
    // #40: the release path uses the dedicated 600s bound; `remove_force`
    // (60s, shared with workspace teardown and GC) is deliberately NOT used
    // here. Both arms stay byte-identical apart from that one argument.
    #[cfg(not(test))]
    let result =
        crate::git_worktree::remove_force_timeout(source_repo, &wt_str, RELEASE_GIT_TIMEOUT);
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

/// The outcome a release route returns after a successful diversion.
///
/// Three routes reach a diversion point and they do NOT share a lock shape:
/// `release_known_locked` returns a `LockedRelease`, while the two exact-target
/// routes return `ReleaseOutcome` and release a different set of guards on the
/// way out — one of them holds a branch lease the other does not. Hand-written,
/// those release sequences drift apart, and lock-order is the one thing on this
/// path that must not drift.
///
/// So the shared part lives here and the caller supplies only what genuinely
/// differs: `release_extra` drops whatever guards this particular route holds
/// beyond the binding and agent locks, which are common to all three.
///
/// The outcome for a release whose required diversion could not complete.
///
/// Fails closed on both counts that matter: `released` stays false so no caller
/// reports success, and the worktree path is carried so the operator is told
/// which directory is still sitting there. The binding is untouched because the
/// route returns before reaching any binding-removal step.
pub(super) fn diversion_failed_outcome(error: &str, worktree: &Path) -> super::ReleaseOutcome {
    let mut out = super::ReleaseOutcome::default();
    mark_release_incomplete(&mut out, "worktree_diversion", worktree, error.to_string());
    out
}

pub(super) fn diverted_release_outcome(
    archive: &Path,
    release_extra: impl FnOnce(),
) -> super::ReleaseOutcome {
    release_extra();
    super::ReleaseOutcome {
        released: true,
        worktree_removed: true,
        path: Some(archive.display().to_string()),
        ..Default::default()
    }
}

pub(super) enum BaselineDecision {
    /// No Unusable journal applies. The route proceeds exactly as before.
    Proceed(Option<BTreeSet<String>>),
    /// The remnant was archived instead of released. `archive` is where the
    /// payload — including everything still on disk — now lives.
    Diverted { archive: PathBuf },
    /// A diversion was required and could NOT be completed: the archive could
    /// not be written, or the rename failed.
    ///
    /// Deliberately distinct from `Proceed(None)`. `Proceed(None)` is not
    /// "stop" — `remove_worktree` treats a `None` baseline by snapshotting the
    /// tree itself and then removing it, so returning it after a failed
    /// diversion would delete the one remaining copy of a damaged worktree.
    /// That is the exact behaviour #39 exists to prevent, so the failure needs
    /// its own arm: the route must report an error and keep both the directory
    /// and the binding.
    DiversionFailed { error: String },
}

/// #39: divert a `WorktreeUnusable` remnant to an archive, else return the
/// tracked-path baseline.
///
/// ## Why the name says "before_snapshot"
///
/// A release route that re-enters with a `WorktreeUnusable` journal must divert
/// **before** anything snapshots or deletes the damaged tree. If the baseline
/// were captured first, the ignored-cache sweep that follows would delete
/// payload and the archive would record a tree that no longer matches what the
/// operator left behind. The ordering is the whole point, so it is in the name:
/// a caller reading this at the call site sees the gate position without
/// opening this file.
///
/// ## Fail-closed
///
/// An unreadable journal, a schema this daemon does not understand, a binding
/// that will not resolve to a known digest, or a journal belonging to an
/// earlier incarnation of a reused name — all mean "do not divert", and the
/// release proceeds exactly as if no journal existed. A corrupt journal is not
/// evidence of damage, and refusing to release on the strength of one would be a
/// worse failure than releasing.
///
/// Note: the sibling helper `clear_if_matches_generation` compares the same
/// digest despite its name; the comparison is
/// `tombstone.binding_sha256 == BindingFingerprint.digest`.
pub(super) fn unusable_divert_before_snapshot(
    home: &Path,
    agent: &str,
    worktree: &Path,
) -> BaselineDecision {
    if !must_divert_unusable(home, agent) {
        return BaselineDecision::Proceed(tracked_path_snapshot(worktree));
    }
    match divert_to_archive(home, agent, worktree) {
        Ok(archive) => BaselineDecision::Diverted { archive },
        Err(error) => {
            // The archive is the only safe destination for a damaged remnant.
            // Failing closed here is the whole point: `Proceed(None)` would NOT
            // stop the release, it would hand `remove_worktree` a `None` baseline
            // that function resolves by snapshotting — and then deleting — the
            // surviving directory.
            tracing::error!(
                agent,
                path = %worktree.display(),
                error = %error,
                "#39: diverting the unusable remnant to an archive failed; the release \
                 is refusing to remove the surviving directory"
            );
            BaselineDecision::DiversionFailed { error }
        }
    }
}

/// True when this release must divert because a `WorktreeUnusable` journal
/// belongs to the binding whose digest is live right now.
fn must_divert_unusable(home: &Path, agent: &str) -> bool {
    let tombstone = match crate::agent::deletion_recovery::read(home, agent) {
        Ok(Some(tombstone)) => tombstone,
        // Absent, or unreadable/unparseable: not evidence of damage.
        Ok(None) | Err(_) => return false,
    };
    if !matches!(
        tombstone.state,
        crate::agent::deletion_recovery::State::WorktreeUnusable { .. }
    ) {
        return false;
    }
    match crate::binding::preflight_guarded_binding(home, agent) {
        crate::binding::GuardedBinding::Known { fingerprint, .. } => {
            tombstone.binding_sha256 == fingerprint.digest
        }
        crate::binding::GuardedBinding::Absent | crate::binding::GuardedBinding::Opaque(_) => false,
    }
}

/// Move the damaged directory into a preservation archive.
///
/// The order is the crash-safety contract and mirrors the admin recovery lane:
/// publish the destination in the journal, write the self-describing metadata
/// inside the source, then rename. A crash before the rename leaves the source
/// intact and the retry re-runnable; after it, the journal already names the
/// destination.
fn divert_to_archive(home: &Path, agent: &str, worktree: &Path) -> Result<PathBuf, String> {
    let archive = crate::admin::archive_mechanics::preservation_directory(home, agent, worktree)?;
    let tombstone = crate::agent::deletion_recovery::read(home, agent)?;
    let (cause, binding_sha256) = match tombstone {
        Some(crate::agent::deletion_recovery::Tombstone {
            state: crate::agent::deletion_recovery::State::WorktreeUnusable { cause },
            binding_sha256,
            ..
        }) => (cause, binding_sha256),
        _ => {
            return Err(format!(
                "diversion requires an unusable tombstone for '{agent}' — the gate condition \
                 did not hold"
            ))
        }
    };
    // The first argument is WHERE THE MANIFEST IS WRITTEN, not where the archive
    // will be. It must be the SOURCE, so the rename carries the evidence into the
    // archive with the payload — the same order the admin lane uses
    // (`write_archive_metadata(&target, …)` then rename). Writing it to
    // `&archive` instead makes that directory non-empty, and renaming a
    // directory onto a non-empty one fails with ENOTEMPTY.
    crate::admin::archive_mechanics::write_preservation_manifest(
        worktree,
        agent,
        "",
        worktree,
        worktree,
        &archive,
        &cause,
        &binding_sha256,
    )?;
    crate::admin::archive_mechanics::rename_worktree_into(worktree, &archive)?;
    Ok(archive)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A real git repo with `src/main.rs` + `src/lib.rs` COMMITTED, so
    /// `tracked_path_snapshot`'s `git ls-files` actually returns paths. A
    /// plain directory is not enough: the snapshot would be `None` and every
    /// tracked-file assertion below would vacuously pass on an empty set.
    fn committed_worktree(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-40-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join(".agend-managed"), "").unwrap();
        dir
    }

    /// #40: the release path gets its OWN 600s bound, not the global
    /// 60s `LOCAL_GIT_TIMEOUT`. `git worktree remove --force` on a large
    /// worktree legitimately outruns 60s, and the 60s kill left the target
    /// half-unlinked. Every other `remove_force` caller (workspace teardown,
    /// GC) keeps the global 60s default — this pins that only the RELEASE
    /// bound moved.
    #[test]
    fn release_git_timeout_is_ten_minutes_not_the_global_local_timeout() {
        assert_eq!(
            super::RELEASE_GIT_TIMEOUT,
            std::time::Duration::from_secs(600),
            "release removal must use the dedicated 600s bound"
        );
        // The dedicated bound must actually be LOOSER than the global one,
        // otherwise the constant is a no-op relabelling.
        assert!(
            super::RELEASE_GIT_TIMEOUT > crate::git_helpers::LOCAL_GIT_TIMEOUT,
            "release bound ({:?}) must exceed the global LOCAL_GIT_TIMEOUT ({:?})",
            super::RELEASE_GIT_TIMEOUT,
            crate::git_helpers::LOCAL_GIT_TIMEOUT
        );
        // The global default is untouched by this change.
        assert_eq!(
            crate::git_helpers::LOCAL_GIT_TIMEOUT,
            std::time::Duration::from_secs(60)
        );
    }

    /// The release arm must call the explicit-bound helpers, and `remove_force`
    /// must keep its original 60s signature so the workspace/GC callers cannot
    /// be silently redirected onto the long bound.
    #[test]
    fn release_routes_through_the_timeout_bound_arm() {
        let _: fn(&[&str], std::time::Duration) -> std::io::Result<std::process::Output> =
            crate::git_helpers::git_bypass_no_cwd_timeout;
        let _: fn(
            &std::path::Path,
            &[&str],
            std::time::Duration,
        ) -> std::io::Result<std::process::Output> = crate::git_helpers::git_bypass_timeout;
        let _: fn(&std::path::Path, &str) -> std::io::Result<std::process::Output> =
            crate::git_worktree::remove_force;
        let _: fn(
            &std::path::Path,
            &str,
            std::time::Duration,
        ) -> std::io::Result<std::process::Output> = crate::git_worktree::remove_force_timeout;
    }

    /// The snapshot the classification depends on really enumerates tracked
    /// files. This is the precondition the two tests below silently rely on;
    /// it fails loudly if the fixture ever stops being a git repo, which would
    /// otherwise make them pass on an empty set.
    #[test]
    fn snapshot_enumerates_tracked_paths_in_a_real_repo() {
        let dir = committed_worktree("snapshot");
        let snapshot =
            super::tracked_path_snapshot(&dir).expect("snapshot must enumerate a git repo");
        assert_eq!(snapshot.len(), 2, "unexpected tracked set: {snapshot:?}");
        assert!(
            snapshot.iter().any(|p| p.ends_with("main.rs")),
            "snapshot missed a committed file: {snapshot:?}"
        );
        assert!(
            snapshot.iter().any(|p| p.ends_with("lib.rs")),
            "snapshot missed a committed file: {snapshot:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A timeout that fires while the directory survives must keep the
    /// EXISTING remnant classification: tracked files still present → plain
    /// `Failed`, not a fabricated `PartiallyRemoved`. #40 lengthens the
    /// deadline only; it must not change what a timeout means.
    #[test]
    fn injected_timeout_on_intact_worktree_still_reports_failed_not_partial() {
        let dir = committed_worktree("intact");

        super::super::release_test_seam::fail_next_remove(std::io::ErrorKind::TimedOut);
        let outcome = super::remove_worktree("impl-agent", &dir, std::path::Path::new(""), None);

        match outcome {
            // Every tracked file survived, so this is an ordinary failure.
            WorktreeRemoval::Failed(message) => {
                assert!(
                    message.contains("git command failed"),
                    "unexpected failure text: {message}"
                );
            }
            other => panic!("expected Failed for an intact worktree, got {other:?}"),
        }

        // Nothing may be deleted on the injected-timeout path.
        assert!(
            dir.join("src/main.rs").exists(),
            "tracked file must survive"
        );
        assert!(dir.join("src/lib.rs").exists(), "tracked file must survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A timeout that fires AFTER tracked files actually disappeared is the
    /// damaged case: it must classify as `PartiallyRemoved` and carry the
    /// missing paths, so the orchestrator never treats it as intact.
    #[test]
    fn injected_timeout_after_tracked_files_vanish_reports_partial_with_missing_paths() {
        let dir = committed_worktree("partial");

        // Snapshot BEFORE anything is unlinked, then delete ONE tracked file
        // so the comparison sees a genuine partial removal.
        let baseline = super::tracked_path_snapshot(&dir).expect("baseline snapshot");
        assert_eq!(baseline.len(), 2, "fixture must start with 2 tracked files");
        std::fs::remove_file(dir.join("src/lib.rs")).unwrap();

        super::super::release_test_seam::fail_next_remove(std::io::ErrorKind::TimedOut);
        let outcome = super::remove_worktree(
            "impl-agent",
            &dir,
            std::path::Path::new(""),
            Some(&baseline),
        );

        match outcome {
            WorktreeRemoval::PartiallyRemoved {
                cause,
                missing_tracked,
            } => {
                assert_eq!(
                    missing_tracked.len(),
                    1,
                    "exactly the vanished file must be reported: {missing_tracked:?}"
                );
                assert!(
                    missing_tracked[0].ends_with("lib.rs"),
                    "the vanished tracked file must be reported: {missing_tracked:?}"
                );
                assert!(
                    cause.contains("git command failed"),
                    "cause should name the failing git command: {cause}"
                );
            }
            other => panic!("expected PartiallyRemoved, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
