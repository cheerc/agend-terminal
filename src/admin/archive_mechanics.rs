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

// ── #39: the preservation lane ───────────────────────────────────────────
//
// These primitives land before their only caller. #39's slice order was
// re-sequenced so this namespace and its metadata shape exist before the
// release-lane diversion that will call them (#39 PR-3); in a non-test build
// nothing calls them yet. The `cfg_attr(not(test), allow(dead_code))` on the
// module declaration in `admin/mod.rs` says so in words at the declaration
// site, so a reviewer sees "no production caller ships yet" rather than a
// silent hole here.

/// Root under which release-lane preservation archives live.
///
/// ## Why not `.trash/worktrees`
///
/// `purge_trash` selects exactly one root — `trash_root(home) =
/// home/.trash/worktrees` (`daemon::retention::worktrees`) — then `read_dir`s
/// that one level and `remove_dir_all`s each entry. It has **no namespace
/// selector**: anything placed beneath that root is a purge candidate, and with
/// `AGEND_WORKTREE_GC_TRASH_DAYS=0` the age comparison is vacuously true, so
/// every sweep removes everything it finds.
///
/// A preservation archive is not disposable: it holds the payload of a
/// half-deleted worktree, which may be the only remaining copy of work nobody
/// has looked at yet. So it lives outside that root entirely rather than relying
/// on "purge only walks the `worktrees` level" — a fact about today's code, not
/// a guarantee.
///
/// The GC candidate walk is likewise rooted elsewhere: `worktree_pool::gc`
/// passes `daemon_managed_worktree_root(home)` and `workspace_dir(home)`, never
/// `home` itself. Nothing under this root is reachable from either sweep.
pub(crate) fn preservation_root(home: &Path) -> PathBuf {
    home.join("recovery-preserved").join("worktrees")
}

/// Reserve a preservation archive directory for a diverted worktree.
///
/// Unlike [`archive_directory`], this does **not** write the journal's
/// `archive` field, and that is deliberate. `binding_state` does not read it,
/// `archive_mechanics` does not read it, and the admin recovery lane only
/// reaches it through `recover_recorded_archive` — whose state gate accepts
/// only `Deleting | RecoveryRequired`, so a `WorktreeUnusable` journal would be
/// refused there anyway. Recording the path in a field nothing can act on would
/// leave an operator holding a pointer to a place they cannot use.
///
/// The journal keeps saying what it is for: this worktree is damaged. The
/// location of the preserved payload rides along with the payload, in the
/// manifest written beside it.
pub(crate) fn preservation_directory(
    home: &Path,
    instance: &str,
    worktree: &Path,
) -> Result<PathBuf, String> {
    let root = preservation_root(home);
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("create preservation root {}: {e}", root.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let directory = root.join(format!(
        "{instance}-diversion-{}-{}",
        stamp.as_secs(),
        stamp.subsec_nanos()
    ));
    if directory.exists() {
        return Err(format!(
            "preservation archive collision: {}",
            directory.display()
        ));
    }
    let _ = worktree;
    Ok(directory)
}

/// Write the manifest that travels with a preserved payload.
///
/// ## What this deliberately omits, and why that is not a lie
///
/// The admin lane's manifest carries `actor` and `audit_reason`. Those fields
/// record **who initiated an operator recovery** — a human typed a command.
/// A diversion is not operator-initiated: it is the release lane noticing a
/// journal that says a previous removal was interrupted. There is no actor to
/// name, and writing a fixed value such as `system` would manufacture one. The
/// field is therefore absent rather than falsified.
///
/// `.agend-recovery-binding.json` and its `.sig` are likewise omitted. Those are
/// the admin lane's replay credentials: `recover_recorded_archive` reads them
/// back to compare against the live binding. A diversion has no binding bytes to
/// copy (and fetching them would reach into admin semantics), and that reader
/// treats a missing file as simply absent rather than as an error.
///
/// What remains is what a human needs to act: which instance, which branch, what
/// the path was, where it now is, and — most importantly — what damage had
/// already been recorded when the diversion fired.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_preservation_manifest(
    directory: &Path,
    instance: &str,
    branch: &str,
    source_repo: &Path,
    original_worktree: &Path,
    archived_worktree: &Path,
    cause: &str,
    binding_sha256: &str,
) -> Result<(), String> {
    let manifest_path = directory.join(".agend-preservation-manifest.json");
    let manifest = serde_json::json!({
        "schema_version": 1,
        "kind": "release_diversion_preservation",
        "instance": instance,
        "branch": branch,
        "source_repo": source_repo,
        "original_worktree": original_worktree,
        "archived_worktree": archived_worktree,
        "damage_cause": cause,
        "binding_sha256": binding_sha256,
        "note": "the worktree was damaged by an interrupted removal; this archive \
                 preserves what remains. The deletion-recovery journal keeps \
                 recording the damage — it does not point here.",
    });
    crate::store::atomic_write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())
}

/// Remove a named preservation archive.
///
/// `target` must resolve inside [`preservation_root`]. That confinement is the
/// whole safety property of this function: it is the only destructive path in
/// the preservation lane, and it refuses to be pointed anywhere else — including
/// at a live worktree, an admin archive, or `.trash`.
///
/// This is explicit cleanup of a named directory. There is no sweep and no
/// expiry: a preservation archive stays until a person removes it.
pub(crate) fn remove_preservation_archive(home: &Path, target: &Path) -> Result<(), String> {
    let root = preservation_root(home);
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("preservation root {} is unavailable: {e}", root.display()))?;
    let canonical_target = target.canonicalize().map_err(|e| {
        format!(
            "preservation archive {} is unavailable: {e}",
            target.display()
        )
    })?;
    if canonical_target == canonical_root || !canonical_target.starts_with(&canonical_root) {
        return Err(format!(
            "preservation cleanup refused: {} is outside {}",
            target.display(),
            canonical_root.display()
        ));
    }
    std::fs::remove_dir_all(&canonical_target).map_err(|e| {
        format!(
            "remove preservation archive {}: {e}",
            canonical_target.display()
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-39-p4-{tag}-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The core claim of #39: a preservation archive must be unreachable by
    /// every existing sweep. This pins it against BOTH of them by name rather
    /// than by appearance — a rename of the root would break this test, which
    /// is the point.
    #[test]
    fn preservation_root_is_outside_every_sweep_39() {
        let home = tmp_home("sweep-root");
        let root = preservation_root(&home);

        // 1. The purge selector is a single hardcoded root. If preservation ever
        //    moved under it, this fails.
        let purge_root = home.join(".trash").join("worktrees");
        assert!(
            !root.starts_with(&purge_root),
            "#39: the preservation root must not live under the purge root \
             {} — purge_trash read_dirs that one level with no namespace selector",
            purge_root.display()
        );
        assert!(
            !purge_root.starts_with(&root),
            "#39: the purge root must not live under the preservation root"
        );

        // 2. The GC candidate walk is rooted at the managed worktree pool and
        //    the workspace dir — never at home itself.
        let managed = crate::worktree_pool::daemon_managed_worktree_root(&home);
        let workspace = crate::paths::workspace_dir(&home);
        for root_dir in [&managed, &workspace] {
            assert!(
                !root.starts_with(root_dir),
                "#39: preservation must not live under a GC scan root ({})",
                root_dir.display()
            );
            assert!(
                !root_dir.starts_with(&root),
                "#39: a GC scan root must not live under preservation ({})",
                root_dir.display()
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// The namespace really is `home/recovery-preserved/worktrees/`, asserted
    /// as a path rather than as a name so a rename cannot pass silently.
    #[test]
    fn preservation_root_is_the_frozen_path_39() {
        let home = tmp_home("root-shape");
        assert_eq!(
            preservation_root(&home),
            home.join("recovery-preserved").join("worktrees")
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The diversion manifest records the damage and the location, and
    /// deliberately omits the operator-attribution fields. If someone later
    /// "helpfully" fills in `actor`, this fails — which is the behaviour we want,
    /// because a diversion has no operator to attribute.
    #[test]
    fn preservation_manifest_records_damage_and_omits_operator_attribution_39() {
        let home = tmp_home("manifest");
        let archive = home.join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        write_preservation_manifest(
            &archive,
            "dev",
            "feat/p4",
            &home.join("repo"),
            &home.join("old-wt"),
            &home.join("archive"),
            "12 tracked files missing (e.g. src/main.rs)",
            "abc123",
        )
        .expect("manifest");

        let raw = std::fs::read_to_string(archive.join(".agend-preservation-manifest.json"))
            .expect("manifest readable");
        let m: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(m["kind"], "release_diversion_preservation");
        assert_eq!(m["instance"], "dev");
        assert_eq!(
            m["damage_cause"],
            "12 tracked files missing (e.g. src/main.rs)"
        );
        assert_eq!(m["binding_sha256"], "abc123");
        assert!(
            m.get("actor").is_none(),
            "#39: a diversion is not operator-initiated; writing an actor would \
             manufacture one. manifest: {raw}"
        );
        assert!(
            m.get("audit_reason").is_none(),
            "#39: same reason — no operator reason exists. manifest: {raw}"
        );
        assert!(
            !archive.join(".agend-recovery-binding.json").exists(),
            "#39: the admin replay credentials must not be written by a diversion"
        );
        // #39 PR-3: the arbitration lists FOUR things a diversion must not
        // write, and the assertion above only covered one filename — a `.sig`
        // written alongside it would have passed unnoticed. This ticket is the
        // first production caller of these primitives, and the moment someone
        // reaches for the neighbouring admin credential shape is exactly when
        // this needs to hold.
        assert!(
            !archive.join(".agend-recovery-binding.json.sig").exists(),
            "#39: the admin replay SIGNATURE must not be written by a diversion either"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Explicit cleanup removes a named archive and refuses anything outside
    /// the preservation root — including a live worktree and the admin archive
    /// root, which is the property that makes it safe to expose at all.
    #[test]
    fn cleanup_is_confined_to_the_preservation_root_39() {
        let home = tmp_home("cleanup");
        let target = preservation_directory(&home, "dev", &home.join("wt")).expect("archive");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("payload.txt"), b"the only copy\n").unwrap();

        // A sibling directory outside the root must be refused, and must survive.
        let outsider = home.join("not-preserved");
        std::fs::create_dir_all(&outsider).unwrap();
        let error = remove_preservation_archive(&home, &outsider).expect_err("must refuse");
        assert!(error.contains("outside"), "unexpected error: {error}");
        assert!(
            outsider.exists(),
            "a refused cleanup must not delete anything"
        );

        // The archive itself is removable.
        remove_preservation_archive(&home, &target).expect("removes a named archive");
        assert!(!target.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Refusing the root itself, not merely its children: `remove_dir_all` on
    /// the root would take every preserved payload with it.
    #[test]
    fn cleanup_refuses_the_preservation_root_itself_39() {
        let home = tmp_home("cleanup-root");
        let archive = preservation_directory(&home, "dev", &home.join("wt")).expect("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let error =
            remove_preservation_archive(&home, &preservation_root(&home)).expect_err("must refuse");
        assert!(error.contains("outside"), "unexpected error: {error}");
        assert!(
            archive.exists(),
            "refusing the root must not remove its contents"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
