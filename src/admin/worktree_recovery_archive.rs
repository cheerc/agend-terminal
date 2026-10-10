use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// #39: these two moved to `admin::archive_mechanics`; the imports keep every
// call site below byte-identical, which is what makes this a refactor rather
// than a change. `SystemTime`/`UNIX_EPOCH` stay here — the archive-path stamp
// in `recover_absent_worktree` is still constructed inline at its call site.
use crate::admin::archive_mechanics::{canonicalize_with_missing_tail, write_archive_metadata};

/// Finish recovery when the release already removed the physical worktree but
/// crashed before clearing its signed binding. The archive is a durable
/// package for the exact binding residue and manifest; there is deliberately
/// no path recreation or Git metadata deletion in this arm.
#[allow(clippy::too_many_arguments)]
pub(super) fn recover_absent_worktree(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    instance: &str,
    branch: &str,
    worktree: &Path,
    source_repo: &Path,
    tombstone: &crate::agent::deletion_recovery::Tombstone,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> Result<super::RecoveryReport, String> {
    // #39: same message correction as the entry point — an Unusable journal is a
    // damaged worktree the release lane still owns, not a pending recovery. The
    // refusal stands and no second exit is opened; only the reason changes.
    if matches!(
        tombstone.state,
        crate::agent::deletion_recovery::State::WorktreeUnusable { .. }
    ) {
        return Err(format!(
            "recovery refused: the worktree is damaged ({}) and operator recovery does not \
             apply. Re-run the release lane (repo action=release) for '{}' to finish it; \
             `binding_state` reports worktree_unusable with the cause.",
            tombstone.worktree, instance
        ));
    }
    if !matches!(
        tombstone.state,
        crate::agent::deletion_recovery::State::Deleting
            | crate::agent::deletion_recovery::State::RecoveryRequired
    ) {
        return Err("recovery refused: tombstone is not pending recovery".to_string());
    }
    if tombstone.instance != instance
        || tombstone.branch != branch
        || Path::new(&tombstone.worktree) != worktree
        || Path::new(&tombstone.source_repo) != source_repo
    {
        return Err(
            "recovery refused: supplied identity does not match the release tombstone".to_string(),
        );
    }
    let managed_root = crate::worktree_pool::daemon_managed_worktree_root(home)
        .canonicalize()
        .map_err(|e| format!("managed worktree root is unavailable: {e}"))?;
    let target = canonicalize_with_missing_tail(worktree)?;
    if !target.starts_with(&managed_root) || target == managed_root {
        return Err(format!(
            "recovery target is outside the daemon worktree root: {}",
            target.display()
        ));
    }
    source_repo
        .canonicalize()
        .map_err(|e| format!("source repository is unavailable: {e}"))?;
    let root = home.join(".trash").join("worktrees");
    std::fs::create_dir_all(&root).map_err(|e| format!("create recovery archive root: {e}"))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("recovery archive root is unavailable: {e}"))?;
    let archive = match tombstone.archive.as_deref() {
        Some(path) => PathBuf::from(path),
        None => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            root.join(format!(
                "{instance}-release-recovery-{}-{}",
                stamp.as_secs(),
                stamp.subsec_nanos()
            ))
        }
    };
    if archive.exists() {
        let canonical = archive
            .canonicalize()
            .map_err(|e| format!("recorded recovery archive is unavailable: {e}"))?;
        if !canonical.starts_with(&root) || canonical == root {
            return Err("recovery refused: archive is outside the recovery root".to_string());
        }
    } else if archive.parent() != Some(root.as_path()) {
        return Err("recovery refused: archive parent is outside the recovery root".to_string());
    }
    std::fs::create_dir_all(&archive)
        .map_err(|e| format!("create recovery archive {}: {e}", archive.display()))?;

    let binding_path = crate::paths::binding_path(home, instance);
    let signature_path = crate::paths::runtime_dir(home)
        .join(instance)
        .join("binding.json.sig");
    let binding_body = std::fs::read(&binding_path)
        .map_err(|e| format!("read binding metadata {}: {e}", binding_path.display()))?;
    let binding_signature = std::fs::read(&signature_path)
        .map_err(|e| format!("read binding signature {}: {e}", signature_path.display()))?;
    if crate::daemon::utils::sha256_hex(&binding_body) != tombstone.binding_sha256
        || crate::daemon::utils::sha256_hex(&binding_signature)
            != tombstone.binding_signature_sha256
    {
        return Err("recovery refused: live binding evidence does not match tombstone".to_string());
    }
    if !crate::binding::signature_valid(home, instance) {
        return Err("recovery refused: binding signature for recovery is not valid".to_string());
    }
    let binding: serde_json::Value = serde_json::from_slice(&binding_body)
        .map_err(|e| format!("recovery refused: binding is invalid JSON: {e}"))?;
    if binding["branch"].as_str() != Some(branch)
        || binding["worktree"].as_str() != Some(worktree.to_string_lossy().as_ref())
        || binding["source_repo"].as_str() != Some(source_repo.to_string_lossy().as_ref())
    {
        return Err("recovery refused: binding identity does not match tombstone".to_string());
    }
    let task_id = binding["task_id"].as_str().unwrap_or_default();
    if !task_id.is_empty() {
        let routed = crate::tasks::load_routed(home, task_id)
            .map_err(|e| format!("recovery refused: task '{task_id}' is unreadable: {e}"))?;
        if !routed.task.status.is_terminal() {
            return Err(format!(
                "recovery refused: task '{task_id}' is still active"
            ));
        }
    }
    crate::agent::deletion_recovery::mark_recovery_required(home, instance, Some(&archive))?;
    write_archive_metadata(
        &archive,
        actor,
        audit_reason,
        instance,
        branch,
        source_repo,
        worktree,
        &archive,
        &binding_body,
        &binding_signature,
    )?;
    match crate::binding::unbind_with_permit(home, instance, permit) {
        crate::binding::BindingRemoval::Removed | crate::binding::BindingRemoval::Absent => {
            let _ = std::fs::remove_dir_all(crate::paths::runtime_dir(home).join(instance));
            crate::agent::deletion_recovery::mark_recovered(home, instance, &archive)?;
            crate::event_log::log(
                home,
                "markerless_worktree_recovered",
                instance,
                &format!(
                    "actor={actor}; branch={branch}; archive={}; reason={audit_reason}",
                    archive.display()
                ),
            );
            Ok(super::RecoveryReport { archive })
        }
        crate::binding::BindingRemoval::Failed(error) => Err(format!(
            "binding removal failed; archive remains at {}: {error}",
            archive.display()
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn recover_recorded_archive(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    instance: &str,
    branch: &str,
    worktree: &Path,
    source_repo: &Path,
    tombstone: &crate::agent::deletion_recovery::Tombstone,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> Result<super::RecoveryReport, String> {
    // #39: same message correction as the entry point — an Unusable journal is a
    // damaged worktree the release lane still owns, not a pending recovery. The
    // refusal stands and no second exit is opened; only the reason changes.
    if matches!(
        tombstone.state,
        crate::agent::deletion_recovery::State::WorktreeUnusable { .. }
    ) {
        return Err(format!(
            "recovery refused: the worktree is damaged ({}) and operator recovery does not \
             apply. Re-run the release lane (repo action=release) for '{}' to finish it; \
             `binding_state` reports worktree_unusable with the cause.",
            tombstone.worktree, instance
        ));
    }
    if !matches!(
        tombstone.state,
        crate::agent::deletion_recovery::State::Deleting
            | crate::agent::deletion_recovery::State::RecoveryRequired
    ) {
        return Err("recovery refused: tombstone is not pending recovery".to_string());
    }
    if tombstone.instance != instance
        || tombstone.branch != branch
        || Path::new(&tombstone.worktree) != worktree
        || Path::new(&tombstone.source_repo) != source_repo
    {
        return Err(
            "recovery refused: supplied identity does not match the delete tombstone".to_string(),
        );
    }
    if worktree.exists() {
        return Err(format!(
            "recovery refused: original worktree reappeared while archive is pending: {}",
            worktree.display()
        ));
    }
    let archive = tombstone
        .archive
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| "recovery refused: pending tombstone has no archive".to_string())?;
    let archive_root = home
        .join(".trash")
        .join("worktrees")
        .canonicalize()
        .map_err(|e| format!("recovery archive root is unavailable: {e}"))?;
    let archive_target = archive
        .canonicalize()
        .map_err(|e| format!("recorded recovery archive is unavailable: {e}"))?;
    if !archive_target.starts_with(&archive_root) || archive_target == archive_root {
        return Err("recovery refused: recorded archive is outside the recovery root".to_string());
    }
    let binding_path = crate::paths::binding_path(home, instance);
    let signature_path = crate::paths::runtime_dir(home)
        .join(instance)
        .join("binding.json.sig");
    let binding_exists = binding_path.is_file();
    let signature_exists = signature_path.is_file();
    let live_binding_body = if binding_exists {
        Some(
            std::fs::read(&binding_path)
                .map_err(|e| format!("read binding metadata {}: {e}", binding_path.display()))?,
        )
    } else {
        None
    };
    let live_signature = if signature_exists {
        Some(
            std::fs::read(&signature_path)
                .map_err(|e| format!("read binding signature {}: {e}", signature_path.display()))?,
        )
    } else {
        None
    };
    if let Some(body) = live_binding_body.as_ref() {
        if crate::daemon::utils::sha256_hex(body) != tombstone.binding_sha256 {
            return Err(
                "recovery refused: live binding evidence does not match tombstone".to_string(),
            );
        }
    }
    if let Some(signature) = live_signature.as_ref() {
        if crate::daemon::utils::sha256_hex(signature) != tombstone.binding_signature_sha256 {
            return Err(
                "recovery refused: live signature evidence does not match tombstone".to_string(),
            );
        }
    }
    if let (Some(_), Some(_)) = (&live_binding_body, &live_signature) {
        if !crate::binding::signature_valid(home, instance) {
            return Err(
                "recovery refused: binding signature for recovery is not valid".to_string(),
            );
        }
    }
    let archived_binding = match std::fs::read(archive.join(".agend-recovery-binding.json")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read archived binding evidence: {error}")),
    };
    let archived_signature = match std::fs::read(archive.join(".agend-recovery-binding.json.sig")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read archived binding signature evidence: {error}")),
    };
    if let Some(binding) = archived_binding.as_ref() {
        if crate::daemon::utils::sha256_hex(binding) != tombstone.binding_sha256 {
            return Err(
                "recovery refused: archived binding evidence does not match tombstone".to_string(),
            );
        }
        if let Some(live_body) = live_binding_body.as_ref() {
            if live_body != binding {
                return Err(
                    "recovery refused: live binding evidence does not match archive".to_string(),
                );
            }
        }
    }
    if let Some(signature) = archived_signature.as_ref() {
        if crate::daemon::utils::sha256_hex(signature) != tombstone.binding_signature_sha256 {
            return Err(
                "recovery refused: archived signature evidence does not match tombstone"
                    .to_string(),
            );
        }
        if let Some(live_signature) = live_signature.as_ref() {
            if live_signature != signature {
                return Err(
                    "recovery refused: live signature evidence does not match archive".to_string(),
                );
            }
        }
    }
    let evidence_binding = live_binding_body
        .as_deref()
        .or(archived_binding.as_deref())
        .ok_or_else(|| "recovery refused: archive has no signed binding evidence".to_string())?;
    let evidence_signature = live_signature
        .as_deref()
        .or(archived_signature.as_deref())
        .ok_or_else(|| "recovery refused: archive has no signed signature evidence".to_string())?;
    let binding: serde_json::Value = serde_json::from_slice(evidence_binding)
        .map_err(|e| format!("recovery refused: archived binding is invalid JSON: {e}"))?;
    if binding["branch"].as_str() != Some(tombstone.branch.as_str())
        || binding["worktree"].as_str() != Some(tombstone.worktree.as_str())
        || binding["source_repo"].as_str() != Some(tombstone.source_repo.as_str())
    {
        return Err(
            "recovery refused: archived binding identity does not match tombstone".to_string(),
        );
    }
    let task_id = binding["task_id"].as_str().unwrap_or_default();
    if !task_id.is_empty() {
        let routed = crate::tasks::load_routed(home, task_id)
            .map_err(|e| format!("recovery refused: task '{task_id}' is unreadable: {e}"))?;
        if !routed.task.status.is_terminal() {
            return Err(format!(
                "recovery refused: task '{task_id}' is still active"
            ));
        }
    }
    if !binding_exists && crate::worktree_pool::is_agent_alive(home, instance) {
        return Err(format!(
            "recovery refused: instance '{instance}' still shows liveness"
        ));
    }
    write_archive_metadata(
        &archive,
        actor,
        audit_reason,
        instance,
        branch,
        source_repo,
        worktree,
        &archive,
        evidence_binding,
        evidence_signature,
    )?;
    match crate::binding::unbind_with_permit(home, instance, permit) {
        crate::binding::BindingRemoval::Removed | crate::binding::BindingRemoval::Absent => {}
        crate::binding::BindingRemoval::Failed(error) => {
            return Err(format!(
                "binding removal failed; archive remains at {}: {error}",
                archive.display()
            ));
        }
    }
    super::remove_empty_dir_tree(
        &crate::worktree_pool::daemon_managed_worktree_root(home).join(instance),
    );
    let _ = std::fs::remove_dir_all(crate::paths::runtime_dir(home).join(instance));
    crate::agent::deletion_recovery::mark_recovered(home, instance, &archive)?;
    crate::event_log::log(
        home,
        "markerless_worktree_recovered",
        instance,
        &format!(
            "actor={actor}; branch={branch}; archive={}; reason={audit_reason}",
            archive.display()
        ),
    );
    Ok(super::RecoveryReport { archive })
}
