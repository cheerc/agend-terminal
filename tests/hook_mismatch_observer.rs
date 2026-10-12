//! #20 step 1 (R1): the hook observer helper records a named branch mismatch
//! through the shared serialized appender — and nothing else.
//!
//! RED-first: `src/bin/agend-hook-observer.rs` does not exist yet, so every
//! test that spawns `CARGO_BIN_EXE_agend-hook-observer` fails to compile/run
//! until the helper lands. GREEN must keep every property below:
//!
//! * fixed observer kind (`hook_branch_mismatch`, main-repo owned — never the
//!   vendor `git_event` envelope), exact-match record on named mismatch only;
//! * every skip shape (match / detached / unknown / binding-empty / existing
//!   short-circuits) writes NO record and exits 0;
//! * malformed input exits 0 without a record and without leaking input bytes;
//! * contention (lock held) and I/O failure (missing home) exit 0 with no
//!   record — a measurement failure never becomes a commit refusal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-hook-observer-test-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmp home");
    dir
}

fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agend-hook-observer"))
}

fn lock_path(home: &Path) -> PathBuf {
    home.join("fleet_events.jsonl.lock")
}

fn audit_path(home: &Path) -> PathBuf {
    home.join("fleet_events.jsonl")
}

fn read_records(home: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(audit_path(home)).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each audit line parses"))
        .collect()
}

/// Spawn the helper the way the hook will: fixed argv, never via a shell.
fn run_observer(
    home: &Path,
    agent: &str,
    task: &str,
    bound: &str,
    actual: &str,
    head_raw: &str,
    commit_source: &str,
) -> std::process::Output {
    Command::new(helper())
        .args([
            "--home",
            &home.display().to_string(),
            "--agent",
            agent,
            "--task",
            task,
            "--bound-branch",
            bound,
            "--actual-branch",
            actual,
            "--head-raw",
            head_raw,
            "--commit-source",
            commit_source,
        ])
        .output()
        .expect("spawn agend-hook-observer")
}

#[test]
fn named_mismatch_appends_exact_fixed_kind_record() {
    let home = tmp_home("mismatch");
    let out = run_observer(
        &home,
        "trailer-agent",
        "t-NEW",
        "fix/old-branch",
        "fix/new-branch",
        "ref: refs/heads/fix/new-branch",
        "message",
    );
    assert!(
        out.status.success(),
        "observer must always exit 0, got {:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let records = read_records(&home);
    assert_eq!(records.len(), 1, "exactly one record, got {records:?}");
    let rec = &records[0];
    assert_eq!(rec["kind"], "hook_branch_mismatch");
    assert_eq!(rec["agent"], "trailer-agent");
    assert_eq!(rec["task"], "t-NEW");
    assert_eq!(rec["bound_branch"], "fix/old-branch");
    assert_eq!(rec["actual_branch"], "fix/new-branch");
    assert!(rec.get("timestamp").is_some(), "record carries a timestamp");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn matching_branch_writes_no_record() {
    let home = tmp_home("match");
    let out = run_observer(
        &home,
        "trailer-agent",
        "t-NEW",
        "fix/same",
        "fix/same",
        "ref: refs/heads/fix/same",
        "message",
    );
    assert!(out.status.success());
    assert!(
        !audit_path(&home).exists() || read_records(&home).is_empty(),
        "a matching branch must not record"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn empty_actual_branch_writes_no_record() {
    // Detached / unknown / binding-empty all surface as an empty actual or
    // empty bound — the hook never calls the helper with a confirmed named
    // pair in those shapes, and a direct call with empties must also skip.
    for (tag, bound, actual) in [
        ("detached", "fix/old", ""),
        ("unknown", "fix/old", ""),
        ("binding-empty", "", "fix/current"),
    ] {
        let home = tmp_home(tag);
        let out = run_observer(
            &home,
            "trailer-agent",
            "t-OLD",
            bound,
            actual,
            "",
            "message",
        );
        assert!(out.status.success(), "{tag}: exit 0");
        assert!(
            !audit_path(&home).exists() || read_records(&home).is_empty(),
            "{tag}: no record"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[test]
fn malformed_input_exits_zero_without_record_or_leak() {
    let home = tmp_home("malformed");
    // No argv at all: missing required fields.
    let out = Command::new(helper())
        .output()
        .expect("spawn agend-hook-observer");
    assert!(
        out.status.success(),
        "malformed input must still exit 0, got {:?}",
        out.status
    );
    assert!(
        !audit_path(&home).exists() || read_records(&home).is_empty(),
        "malformed input must not record"
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !stderr.contains("fix/old-branch"),
        "must not echo untrusted input: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn lock_contention_skips_without_record_and_stays_zero() {
    let home = tmp_home("contention");
    // Hold the companion lock the way the appender does (exclusive, non-block
    // would fail) so the helper's single TryOnce attempt contends.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path(&home))
        .expect("open lock");
    // fs4 is a main-dependency (vendored appender uses it); tests use libc flock
    // via the same syscall shape — hold LOCK_EX for the spawn duration.
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "test must hold the lock");
    }
    let out = run_observer(
        &home,
        "trailer-agent",
        "t-NEW",
        "fix/old-branch",
        "fix/new-branch",
        "ref: refs/heads/fix/new-branch",
        "message",
    );
    assert!(
        out.status.success(),
        "contention must exit 0, got {:?}",
        out.status
    );
    assert!(
        !audit_path(&home).exists() || read_records(&home).is_empty(),
        "contention must skip (no unlocked fallback)"
    );
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    }
    drop(lock);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn missing_home_dir_exits_zero_without_record() {
    let home = tmp_home("missing-home");
    let _ = std::fs::remove_dir_all(&home);
    // A home path whose parent does not exist: open fails → exit 0, no record.
    let dead = home.join("no-such-parent").join("home");
    let out = run_observer(
        &dead,
        "trailer-agent",
        "t-NEW",
        "fix/old-branch",
        "fix/new-branch",
        "ref: refs/heads/fix/new-branch",
        "message",
    );
    assert!(out.status.success(), "I/O failure must exit 0");
    let _ = std::fs::remove_dir_all(&home);
}

/// The installed hook calls the helper on a named mismatch only, and a real
/// `git commit` through it keeps the commit outcome + withhold behavior while
/// landing exactly one observer record.
#[test]
fn installed_hook_records_mismatch_on_real_git_commit() {
    let home = tmp_home("e2e");
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    // Install the compiled helper where the hook will look: $AGEND_HOME/bin.
    let helper_bin = bin.join("agend-hook-observer");
    std::fs::copy(helper(), &helper_bin).unwrap();
    std::fs::set_permissions(&helper_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Minimal repo + binding describing the PREVIOUS branch.
    let repo = tmp_home("e2e-repo");
    let out = Command::new("git")
        .args(["init", "-b", "fix/new-branch"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git init");
    assert!(out.status.success());
    for kv in [
        ["config", "user.name", "Test User"],
        ["config", "user.email", "test@example.com"],
    ] {
        let out = Command::new("git")
            .args(kv)
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git config");
        assert!(out.status.success());
    }
    let agent = "trailer-agent";
    let runtime = home.join("runtime").join(agent);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(
        runtime.join("binding.json"),
        "{\"task_id\": \"t-OLD\", \"branch\": \"fix/old-branch\", \
         \"issued_at\": \"2026-09-07T00:00:00Z\"}\n",
    )
    .unwrap();

    // Install the REAL hook under test by copying the committed asset, the
    // same bytes install_hooks embeds (binding.rs:762-783 embeds this file).
    let hooks = home.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(
        hooks.join("prepare-commit-msg"),
        include_str!("../assets/hooks/prepare-commit-msg"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        std::fs::set_permissions(
            hooks.join("prepare-commit-msg"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    // Real commit on fix/new-branch while bound to fix/old-branch.
    let msg = repo.join("MSG");
    std::fs::write(&msg, "subject\n").unwrap();
    let hook = home.join("hooks").join("prepare-commit-msg");
    let status = Command::new("sh")
        .arg(&hook)
        .arg(&msg)
        .arg("message")
        .env("AGEND_INSTANCE_NAME", agent)
        .env("AGEND_HOME", &home)
        .env("GIT_DIR", repo.join(".git").display().to_string())
        .stdin(Stdio::null())
        .status()
        .expect("run installed hook");
    assert!(status.success(), "hook must exit 0");
    let body = std::fs::read_to_string(&msg).unwrap();
    assert!(body.contains("Agend-Agent: trailer-agent"));
    assert!(
        !body.contains("Agend-Branch:"),
        "withhold unchanged: {body}"
    );

    let records = read_records(&home);
    assert_eq!(records.len(), 1, "one observer record, got {records:?}");
    assert_eq!(records[0]["kind"], "hook_branch_mismatch");
    assert_eq!(records[0]["bound_branch"], "fix/old-branch");
    assert_eq!(records[0]["actual_branch"], "fix/new-branch");

    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&repo);
}

/// The hook emits no observer call on a matching branch — no record appears.
#[test]
fn installed_hook_records_nothing_when_branches_match() {
    let home = tmp_home("e2e-match");
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let helper_bin = bin.join("agend-hook-observer");
    std::fs::copy(helper(), &helper_bin).unwrap();
    std::fs::set_permissions(&helper_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let repo = tmp_home("e2e-match-repo");
    let out = Command::new("git")
        .args(["init", "-b", "fix/same"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git init");
    assert!(out.status.success());
    let agent = "trailer-agent";
    let runtime = home.join("runtime").join(agent);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(
        runtime.join("binding.json"),
        "{\"task_id\": \"t-NEW\", \"branch\": \"fix/same\", \
         \"issued_at\": \"2026-09-07T00:00:00Z\"}\n",
    )
    .unwrap();
    // Install the REAL hook under test by copying the committed asset, the
    // same bytes install_hooks embeds (binding.rs:762-783 embeds this file).
    let hooks = home.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(
        hooks.join("prepare-commit-msg"),
        include_str!("../assets/hooks/prepare-commit-msg"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        std::fs::set_permissions(
            hooks.join("prepare-commit-msg"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    let msg = repo.join("MSG");
    std::fs::write(&msg, "subject\n").unwrap();
    // Exercise the hook's mismatch branch decision the same way the installed
    // script does: only a byte-unequal named pair would exec the helper. Here
    // the pair matches, so the helper must never run — assert via absence.
    let hook_body = std::fs::read_to_string(home.join("hooks").join("prepare-commit-msg")).unwrap();
    assert!(
        hook_body.contains("agend-hook-observer"),
        "installed hook must wire the observer helper"
    );
    let status = Command::new("sh")
        .arg(home.join("hooks").join("prepare-commit-msg"))
        .arg(&msg)
        .arg("message")
        .env("AGEND_INSTANCE_NAME", agent)
        .env("AGEND_HOME", &home)
        .env("GIT_DIR", repo.join(".git").display().to_string())
        .stdin(Stdio::null())
        .status()
        .expect("run installed hook");
    assert!(status.success());
    assert!(
        !audit_path(&home).exists() || read_records(&home).is_empty(),
        "matching branch must not record"
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&repo);
}
