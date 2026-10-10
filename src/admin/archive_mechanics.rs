//! #39: the mechanical half of archiving a bound worktree.
//!
//! A recovery archive is a `fs::rename` of the worktree directory plus the
//! self-describing metadata that must land atomically with it. That is the
//! whole of the mechanism. What an archive MEANS — whether the instance is
//! alive, whether its task is terminal, whether the journal may be settled, who
//! is permitted to do it — belongs to the caller, not here.
//!
//! ## Why this is not `retention::worktrees::try_archive`
//!
//! That function shares a name and little else. On `EXDEV` it falls back to
//! `copy_dir_recursive` + `remove_dir_all`, and its own doc records that the
//! fallback is **TOCTOU-unsafe** — the source may change during the copy, and a
//! failure can leave a partial duplicate. That is an acceptable trade for a
//! retention sweep, which is best-effort by nature. It is NOT acceptable for a
//! recovery archive, which must move the payload intact or move nothing at all.
//!
//! [`rename_worktree_into`] therefore has no fallback whatsoever: a cross-device
//! rename simply fails, and the caller keeps the worktree exactly where it was.
//! The two mechanisms are deliberately not interchangeable — do not unify them.
//!
//! ## Known cost of this shape
//!
//! This module does **not** test for `EXDEV` specially, so a cross-device rename
//! and any other rename failure travel the same error path. That is deliberate:
//! this is a pure refactor of existing admin-recovery behaviour, which already
//! had no `EXDEV` branch. A caller that needs to *distinguish* cross-device from
//! other failures would have to add that branch — which is a behaviour change,
//! not a refactor, and must be argued for on its own merits.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Move `worktree` to `archive` with a plain rename and no fallback.
///
/// A cross-device rename fails like any other rename failure: nothing is copied,
/// nothing is deleted, and `worktree` is left untouched. This is the property
/// that separates a recovery archive from a retention sweep.
pub(crate) fn rename_worktree_into(worktree: &Path, archive: &Path) -> Result<(), String> {
    std::fs::rename(worktree, archive).map_err(|e| {
        format!(
            "archive rename {} -> {} failed: {e}",
            worktree.display(),
            archive.display()
        )
    })
}

/// Resolve a path whose tail may not exist yet, keeping the existing prefix
/// canonical. `fs::canonicalize` fails on a missing leaf, so the missing
/// components are peeled off, the deepest existing ancestor is canonicalized,
/// and the peeled components are appended back in order.
pub(crate) fn canonicalize_with_missing_tail(path: &Path) -> Result<PathBuf, String> {
    let mut missing = Vec::new();
    let mut existing = path.to_path_buf();
    while !existing.exists() {
        let component = existing
            .file_name()
            .ok_or_else(|| format!("path has no canonicalizable parent: {}", path.display()))?;
        missing.push(component.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| format!("path has no canonicalizable parent: {}", path.display()))?
            .to_path_buf();
    }
    let mut canonical = existing
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", existing.display()))?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

/// #39: the archive directory layout, isolated so no caller hard-codes a root.
///
/// The recovery lane's preservation namespace (`home/recovery-preserved/…`) is
/// chosen by a later slice; this function takes the root as a parameter so that
/// decision stays in one place instead of being smeared through the callers.
///
/// `suffix` distinguishes the two archive intents that share one root today —
/// an operator-initiated recovery versus a release completing one.
pub(crate) fn archive_directory(
    home: &Path,
    instance: &str,
    suffix: &str,
) -> Result<PathBuf, String> {
    let root = home.join(".trash").join("worktrees");
    std::fs::create_dir_all(&root).map_err(|e| format!("create recovery archive root: {e}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let directory = root.join(format!(
        "{instance}-{suffix}-{}-{}",
        stamp.as_secs(),
        stamp.subsec_nanos()
    ));
    if directory.exists() {
        return Err(format!(
            "recovery archive collision: {}",
            directory.display()
        ));
    }
    Ok(directory)
}

/// Write the self-describing metadata that must travel with the payload.
///
/// The metadata is written BEFORE the rename so the rename carries it
/// atomically. Each file is idempotent — an existing file is compared and only
/// written when absent — so a retry that resumes after an interruption does not
/// rewrite evidence and does not collide with itself.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_archive_metadata(
    directory: &Path,
    actor: &str,
    audit_reason: &str,
    instance: &str,
    branch: &str,
    source_repo: &Path,
    original_worktree: &Path,
    archived_worktree: &Path,
    binding_body: &[u8],
    binding_signature: &[u8],
) -> Result<(), String> {
    let binding_path = directory.join(".agend-recovery-binding.json");
    if let Ok(existing) = std::fs::read(&binding_path) {
        if existing != binding_body {
            return Err(format!(
                "recovery metadata collision at {}",
                binding_path.display()
            ));
        }
    }
    let signature_path = directory.join(".agend-recovery-binding.json.sig");
    if let Ok(existing) = std::fs::read(&signature_path) {
        if existing != binding_signature {
            return Err(format!(
                "recovery signature metadata collision at {}",
                signature_path.display()
            ));
        }
    }
    let manifest_path = directory.join(".agend-recovery-manifest.json");
    let manifest = serde_json::json!({
        "schema_version": 1,
        "actor": actor,
        "audit_reason": audit_reason,
        "instance": instance,
        "branch": branch,
        "source_repo": source_repo,
        "original_worktree": original_worktree,
        "archived_worktree": archived_worktree,
        "binding_sha256": crate::daemon::utils::sha256_hex(binding_body),
    });
    if let Ok(existing) = std::fs::read(&manifest_path) {
        let existing: serde_json::Value = serde_json::from_slice(&existing)
            .map_err(|e| format!("parse existing recovery manifest: {e}"))?;
        if existing["instance"] != instance
            || existing["archived_worktree"] != archived_worktree.to_string_lossy().as_ref()
        {
            return Err(format!(
                "recovery manifest metadata collision at {}",
                manifest_path.display()
            ));
        }
    }
    if !binding_path.is_file() {
        crate::store::atomic_write(&binding_path, binding_body).map_err(|e| e.to_string())?;
    }
    if !signature_path.is_file() {
        crate::store::atomic_write(&signature_path, binding_signature)
            .map_err(|e| e.to_string())?;
    }
    if !manifest_path.is_file() {
        crate::store::atomic_write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
