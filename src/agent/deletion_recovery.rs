//! Durable delete/recovery fences for bound worktrees.
//!
//! The in-memory deleting set protects one daemon process.  This sidecar keeps
//! the same name fenced across a daemon restart when teardown had to retain a
//! markerless bound worktree for operator recovery.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
thread_local! {
    static FORCE_CLEAR_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) struct ClearFailureGuard;

#[cfg(test)]
impl Drop for ClearFailureGuard {
    fn drop(&mut self) {
        FORCE_CLEAR_FAILURE.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
pub(crate) fn force_clear_failure() -> ClearFailureGuard {
    FORCE_CLEAR_FAILURE.with(|flag| flag.set(true));
    ClearFailureGuard
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Deleting,
    RecoveryRequired,
    Recovered,
    /// #40: the worktree removal was KILLED part-way, so the directory
    /// survives with some tracked files already deleted. The binding is
    /// deliberately left in place (the remnant must stay attributable) but is
    /// no longer usable, and this state is what makes that visible to the
    /// agent and the orchestrator through `binding_state`.
    WorktreeUnusable {
        /// Why the removal stopped, for the operator.
        cause: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Tombstone {
    pub(crate) schema_version: u32,
    pub(crate) state: State,
    pub(crate) instance: String,
    pub(crate) branch: String,
    pub(crate) worktree: String,
    pub(crate) source_repo: String,
    pub(crate) binding_sha256: String,
    pub(crate) binding_signature_sha256: String,
    #[serde(default)]
    pub(crate) archive: Option<String>,
}

pub(crate) fn path(home: &Path, instance: &str) -> PathBuf {
    home.join("deletion-recovery")
        .join(format!("{instance}.json"))
}

/// #39: the operator-facing guidance for a name this journal currently fences.
///
/// Every site that refuses on a pending fence reports this same dead end, and
/// each one used to name only the condition — so an operator had to already know
/// that `deletion-recovery/<name>.json` exists, that `binding_state` explains its
/// state, and that `admin recover-worktree` can finish or inspect the release.
/// None of that appeared anywhere the refusal was reported. The message lives
/// here, next to [`path`] and [`State`], so all three sites stay in lockstep and
/// `lifecycle.rs` does not grow a third copy of the same prose.
pub(crate) fn fence_guidance(home: &Path, instance: &str) -> String {
    format!(
        "#39 journal path: {}. Inspect with `binding_state`; finish or inspect the \
         release with `agend-terminal admin recover-worktree`",
        path(home, instance).display()
    )
}

/// #39: the delete-entry refusal for a name this journal already fences.
///
/// It also reports the tombstone's `state`, because this refusal is the
/// operator's dead end — `recover-worktree` ALSO refuses while the managed
/// marker survives — so it is the only place that can distinguish "a delete is
/// in progress" from "a removal was interrupted and needs archiving".
pub(crate) fn describe_pending_delete(
    home: &Path,
    instance: &str,
    tombstone: &Tombstone,
) -> String {
    format!(
        "recovery_required: instance '{instance}' already has a pending delete tombstone \
         (state={:?}, worktree={}). {}",
        tombstone.state,
        tombstone.worktree,
        fence_guidance(home, instance)
    )
}

pub(crate) fn read(home: &Path, instance: &str) -> Result<Option<Tombstone>, String> {
    let path = path(home, instance);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "read deletion tombstone {}: {error}",
                path.display()
            ));
        }
    };
    let tombstone: Tombstone = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse deletion tombstone {}: {error}", path.display()))?;
    if tombstone.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported deletion tombstone schema {} (expected {})",
            tombstone.schema_version, SCHEMA_VERSION
        ));
    }
    Ok(Some(tombstone))
}

pub(crate) fn is_blocking(home: &Path, instance: &str) -> bool {
    let path = path(home, instance);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Tombstone>(&bytes)
            .map(|tombstone| {
                tombstone.schema_version != SCHEMA_VERSION || tombstone.state != State::Recovered
            })
            .unwrap_or(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

pub(crate) fn begin_from_binding(home: &Path, instance: &str) -> Result<Option<Tombstone>, String> {
    let binding_path = crate::paths::binding_path(home, instance);
    let binding_body = match std::fs::read(&binding_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read binding for deletion fence: {error}")),
    };
    let binding: Value = serde_json::from_slice(&binding_body)
        .map_err(|error| format!("recovery_required: binding is not valid JSON: {error}"))?;
    let object = binding
        .as_object()
        .ok_or_else(|| "recovery_required: binding is not an object".to_string())?;
    let branch = object
        .get("branch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "recovery_required: binding branch is missing".to_string())?;
    let worktree = object
        .get("worktree")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "recovery_required: binding worktree is missing".to_string())?;
    // Legacy test/compatibility bindings predating signed source identity do
    // not participate in this recovery lane; the normal release path keeps
    // its existing behavior for them.  Production task bindings always carry
    // source_repo and a signature, which is the only state this tombstone can
    // safely preserve for operator recovery.
    let Some(source_repo) = object
        .get("source_repo")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let signature_path = crate::paths::runtime_dir(home)
        .join(instance)
        .join("binding.json.sig");
    let signature = std::fs::read(&signature_path)
        .map_err(|error| format!("recovery_required: read binding signature: {error}"))?;
    if !crate::binding::signature_valid(home, instance) {
        return Err("recovery_required: binding signature is invalid".to_string());
    }

    let tombstone = Tombstone {
        schema_version: SCHEMA_VERSION,
        state: State::Deleting,
        instance: instance.to_string(),
        branch: branch.to_string(),
        worktree: worktree.to_string(),
        source_repo: source_repo.to_string(),
        binding_sha256: crate::daemon::utils::sha256_hex(&binding_body),
        binding_signature_sha256: crate::daemon::utils::sha256_hex(&signature),
        archive: None,
    };
    // A release can be retried after a daemon restart, and a delete handler can
    // re-enter the same boundary while its lifecycle fence is still pending.
    // Preserve the first exact signed intent instead of replacing it with a
    // fresh timestamp or silently accepting a different target identity.
    if let Some(existing) = read(home, instance)? {
        if existing.state != State::Recovered {
            if existing.instance == tombstone.instance
                && existing.branch == tombstone.branch
                && existing.worktree == tombstone.worktree
                && existing.source_repo == tombstone.source_repo
                && existing.binding_sha256 == tombstone.binding_sha256
                && existing.binding_signature_sha256 == tombstone.binding_signature_sha256
            {
                return Ok(Some(existing));
            }
            return Err(
                "recovery_required: existing tombstone identity does not match binding".to_string(),
            );
        }
    }
    write(home, &tombstone)?;
    Ok(Some(tombstone))
}

/// #39: mark the journal as pending recovery, preserving an existing
/// `WorktreeUnusable` state.
///
/// This is the SINGLE convergence point for every production transition into
/// `RecoveryRequired` — the Delete lane (`instance_state/lifecycle.rs`), and the
/// four release-lane call sites through `mark_release_recovery_required`. Guarding
/// here therefore protects all of them at once, rather than scattering the same
/// check at each site (which is what would make it impossible to tell later which
/// copies were deliberate).
///
/// Why it matters: `binding_state` reports the operator's ONLY view of the damage
/// source through `worktree_unusable { worktree, cause }`, which is non-null only
/// for `WorktreeUnusable`. Downgrading the state to `RecoveryRequired` does not
/// just relabel it — it erases `cause`, so an operator whose release failed
/// afterwards can no longer learn what was already destroyed.
///
/// The tombstone is PERSISTENT state that outlives any single release attempt:
/// "this attempt took the Removed branch" does not imply "the journal is not
/// already Unusable" from an earlier attempt. That earlier state is exactly what
/// an operator needs preserved. So the transition is refused here rather than at
/// the call sites.
///
/// Deliberately NOT done: no new state, no journal field, no repair path. When a
/// release later succeeds it calls `clear`, which removes the journal outright;
/// by then the damage source has been either archived (a later slice) or
/// explicitly superseded by the operator.
pub(crate) fn mark_recovery_required(
    home: &Path,
    instance: &str,
    archive: Option<&Path>,
) -> Result<(), String> {
    let mut tombstone = read(home, instance)?
        .ok_or_else(|| "recovery_required: delete tombstone is missing".to_string())?;
    if matches!(tombstone.state, State::WorktreeUnusable { .. }) {
        // Preserve both the state and `cause`. Returning Ok (not Err) keeps every
        // existing caller's control flow intact — none of them treats this as a
        // failure today, and making it one would newly fail a delete that
        // currently succeeds. The durable record simply stops changing here.
        return Ok(());
    }
    tombstone.state = State::RecoveryRequired;
    tombstone.archive = archive.map(|path| path.display().to_string());
    write(home, &tombstone)
}

/// #40: record that a worktree removal was killed mid-walk and the directory
/// survives in a damaged state. Idempotent — re-recording keeps the first
/// cause, because the FIRST interruption is the one that explains the damage.
pub(crate) fn mark_worktree_unusable(
    home: &Path,
    instance: &str,
    cause: &str,
) -> Result<(), String> {
    let Some(mut tombstone) = read(home, instance)? else {
        // A new release should have entered the signed recovery lane before
        // it starts mutating the worktree. If that invariant is missing, do
        // NOT silently pretend the unusable state was recorded.
        return Err("worktree_unusable: deletion-recovery tombstone is missing".to_string());
    };
    if matches!(tombstone.state, State::WorktreeUnusable { .. }) {
        return Ok(());
    }
    tombstone.state = State::WorktreeUnusable {
        cause: cause.to_string(),
    };
    write(home, &tombstone)
}

pub(crate) fn mark_recovered(home: &Path, instance: &str, archive: &Path) -> Result<(), String> {
    let mut tombstone = read(home, instance)?
        .ok_or_else(|| "recovery_required: delete tombstone is missing".to_string())?;
    tombstone.state = State::Recovered;
    tombstone.archive = Some(archive.display().to_string());
    write(home, &tombstone)
}

pub(crate) fn clear(home: &Path, instance: &str) -> Result<(), String> {
    #[cfg(test)]
    if FORCE_CLEAR_FAILURE.with(std::cell::Cell::get) {
        return Err("forced deletion tombstone clear failure".to_string());
    }
    let path = path(home, instance);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove deletion tombstone {}: {error}",
            path.display()
        )),
    }
}

fn write(home: &Path, tombstone: &Tombstone) -> Result<(), String> {
    let path = path(home, &tombstone.instance);
    let body = serde_json::to_vec_pretty(tombstone)
        .map_err(|error| format!("serialize deletion tombstone: {error}"))?;
    crate::store::atomic_write(&path, &body)
        .map_err(|error| format!("write deletion tombstone {}: {error}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-39-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plant a tombstone in `state` without going through a real signed binding —
    /// the guard under test reads the journal, not the signature.
    fn plant(home: &Path, instance: &str, state: State) {
        write(
            home,
            &Tombstone {
                schema_version: SCHEMA_VERSION,
                state,
                instance: instance.to_string(),
                branch: "feat/x".to_string(),
                worktree: "/tmp/wt".to_string(),
                source_repo: "/tmp/repo".to_string(),
                binding_sha256: "a".repeat(64),
                binding_signature_sha256: "b".repeat(64),
                archive: None,
            },
        )
        .unwrap();
    }

    fn state_of(home: &Path, instance: &str) -> State {
        read(home, instance).unwrap().unwrap().state
    }

    /// #39: the core guarantee. An Unusable journal keeps its state AND its cause
    /// when a later release lane calls `mark_recovery_required` — because
    /// `binding_state` reports the damage source ONLY through this state, so
    /// downgrading it would erase the operator's only view of what was destroyed.
    #[test]
    fn unusable_survives_a_later_recovery_required_transition_39() {
        let home = tmp_home("unusable-preserved");
        plant(
            &home,
            "agent",
            State::WorktreeUnusable {
                cause: "12 tracked files missing (e.g. src/main.rs)".to_string(),
            },
        );

        // The Delete lane and all four release-lane call sites converge here.
        mark_recovery_required(&home, "agent", None).expect("transition must not error");

        let tombstone = read(&home, "agent").unwrap().unwrap();
        assert!(
            matches!(tombstone.state, State::WorktreeUnusable { .. }),
            "#39: a later RecoveryRequired transition must not downgrade Unusable — \
             that erases the operator's only view of the damage source. got {:?}",
            tombstone.state
        );
        assert_eq!(
            tombstone.state,
            State::WorktreeUnusable {
                cause: "12 tracked files missing (e.g. src/main.rs)".to_string()
            },
            "#39: the cause must survive byte-for-byte"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The guard must return Ok, not Err: every existing caller continues on this
    /// path today, so turning it into a failure would newly fail a delete that
    /// currently succeeds.
    #[test]
    fn unusable_transition_is_a_no_op_not_an_error_39() {
        let home = tmp_home("unusable-noop");
        plant(
            &home,
            "agent",
            State::WorktreeUnusable {
                cause: "c".to_string(),
            },
        );
        let before = std::fs::read(path(&home, "agent")).unwrap();
        mark_recovery_required(&home, "agent", None).expect("must be Ok");
        assert_eq!(
            std::fs::read(path(&home, "agent")).unwrap(),
            before,
            "#39: the journal must not even be rewritten — no needless write race"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #39: an archive argument must not be able to launder an Unusable journal
    /// either. Admin recovery passes `Some(&archive)`; although its own
    /// precondition currently excludes Unusable, the convergence point must not
    /// depend on every caller's precondition to hold.
    #[test]
    fn unusable_is_preserved_even_when_an_archive_is_supplied_39() {
        let home = tmp_home("unusable-with-archive");
        plant(
            &home,
            "agent",
            State::WorktreeUnusable {
                cause: "c".to_string(),
            },
        );
        let archive = home.join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        mark_recovery_required(&home, "agent", Some(&archive)).expect("must be Ok");
        assert!(
            matches!(state_of(&home, "agent"), State::WorktreeUnusable { .. }),
            "#39: supplying an archive must not downgrade Unusable at the convergence point"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Non-Unusable states keep their existing behaviour exactly: the transition
    /// still happens, and `archive` still records.
    #[test]
    fn non_unusable_states_still_transition_39() {
        for (tag, start) in [
            ("deleting", State::Deleting),
            ("recovery", State::RecoveryRequired),
        ] {
            let home = tmp_home(tag);
            plant(&home, "agent", start.clone());
            let archive = home.join("archive");
            std::fs::create_dir_all(&archive).unwrap();
            mark_recovery_required(&home, "agent", Some(&archive)).expect("must be Ok");
            let tombstone = read(&home, "agent").unwrap().unwrap();
            assert_eq!(
                tombstone.state,
                State::RecoveryRequired,
                "#39: {tag:?} must keep transitioning to RecoveryRequired"
            );
            assert_eq!(
                tombstone.archive.as_deref(),
                Some(archive.display().to_string().as_str()),
                "#39: {tag:?} must still record the archive"
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// A missing journal still errors — unchanged behaviour, pinned so the guard
    /// cannot be mistaken for a blanket success.
    #[test]
    fn missing_tombstone_still_errors_39() {
        let home = tmp_home("missing");
        let error = mark_recovery_required(&home, "ghost", None).expect_err("must still error");
        assert!(
            error.contains("delete tombstone is missing"),
            "unchanged: {error}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
