//! #3669: shared three-dimensional JSONL retention for daemon-owned stores.
//!
//! Every JSONL store under `$AGEND_HOME` gets the same three caps, enforced
//! on write under that file's companion advisory lock:
//! live-byte cap → generation count → max age. This is the
//! `mcp::usage_stats` pattern (`append_line_with_policy` → rotate + prune),
//! generalized over the file path so each owner keeps its own policy values
//! while sharing one implementation.
//!
//! ## Retention table (single discoverability place)
//!
//! | store | owner / writer | live | gens | age | enforcement |
//! |---|---|---|---|---|---|
//! | `fleet_events.jsonl` | `agentic-audit-append` (vendored crate) | 10 MB | 5 | 30 d | on append, companion lock |
//! | `state-transitions.jsonl` | `daemon::usage_limit::log_state_transition_at` | 10 MB | 5 | 30 d | on append, companion lock |
//! | `mcp-usage-stats.jsonl` | `mcp::usage_stats` (pre-existing) | 1 MB | 5 | 30 d | on append, own lock |
//! | `event-log.jsonl` | `event_log` (pre-existing) | 10 MB | 5 | — | on append, own lock |
//! | `hardwrap_miss_shadow.jsonl` | ORPHANED — writer retired (#2291), frozen | — | — | — | report-only, no writer to enforce on |
//! | `unclassified_errors.jsonl` | ORPHANED — writer retired, frozen | — | — | — | report-only, no writer to enforce on |
//!
//! Lock discipline: rotation runs INSIDE the same companion-lock hold as the
//! append, so a concurrent appender can neither interleave a record into the
//! file being renamed nor miss the size check (no check-then-act race). The
//! lock used here is always [`crate::store::acquire_file_lock`] — the sole
//! flock chokepoint — never a raw fs4 flock (see
//! `tests/flock_depth_invariant.rs`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The three retention dimensions for one JSONL store.
#[derive(Clone, Copy)]
pub struct RetentionPolicy {
    /// Rotate the live file once it exceeds this many bytes.
    pub max_live_bytes: u64,
    /// Keep at most this many rotated generations (`.1` .. `.N`).
    pub max_rotated_files: usize,
    /// Drop rotated generations older than this (by file mtime).
    pub max_rotated_age: Duration,
}

/// Append one JSON value as a line and enforce `policy` (rotate + prune)
/// before returning, all under the file's companion advisory lock.
pub fn append_line_with_retention(
    path: &Path,
    line: &serde_json::Value,
    policy: RetentionPolicy,
) -> std::io::Result<()> {
    use std::io::Write;
    let lock_path = path.with_extension("jsonl.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path).map_err(std::io::Error::other)?;

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")?;
    drop(f);

    rotate_if_needed(path, policy);
    prune_rotated(path, policy, SystemTime::now());
    Ok(())
}

fn rotated_path(base: &Path, gen: usize) -> PathBuf {
    let mut name = base.file_name().map(|s| s.to_owned()).unwrap_or_default();
    name.push(format!(".{gen}"));
    base.with_file_name(name)
}

fn rotate_if_needed(path: &Path, policy: RetentionPolicy) {
    if policy.max_rotated_files == 0 {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= policy.max_live_bytes {
        return;
    }

    let _ = std::fs::remove_file(rotated_path(path, policy.max_rotated_files));
    for gen in (1..policy.max_rotated_files).rev() {
        let src = rotated_path(path, gen);
        let dst = rotated_path(path, gen + 1);
        if src.exists() {
            let _ = std::fs::rename(src, dst);
        }
    }
    if std::fs::rename(path, rotated_path(path, 1)).is_ok() {
        let _ = std::fs::File::create(path);
    }
}

fn prune_rotated(path: &Path, policy: RetentionPolicy, now: SystemTime) {
    let Some(dir) = path.parent() else {
        return;
    };
    let Some(base_name) = path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let prefix = format!("{base_name}.");

    let mut rotated: Vec<(PathBuf, usize, SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let gen = name.strip_prefix(&prefix)?.parse::<usize>().ok()?;
            let mtime = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((path, gen, mtime))
        })
        .collect();

    for (path, gen, mtime) in &rotated {
        let too_old = now
            .duration_since(*mtime)
            .map(|age| age > policy.max_rotated_age)
            .unwrap_or(false);
        if *gen > policy.max_rotated_files || too_old {
            let _ = std::fs::remove_file(path);
        }
    }

    rotated.retain(|(path, gen, mtime)| {
        path.exists()
            && *gen <= policy.max_rotated_files
            && now
                .duration_since(*mtime)
                .map(|age| age <= policy.max_rotated_age)
                .unwrap_or(true)
    });
    rotated.sort_by_key(|(_, gen, _)| *gen);
    for (idx, (path, _, _)) in rotated.iter().enumerate() {
        if idx >= policy.max_rotated_files {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// #3669: hourly safety-net sweep over the daemon-owned stores' rotated
/// generations (called from the log-rotation tick). Enforces each store's
/// generation + age caps WITHOUT touching the live `.jsonl` file itself —
/// the live file only ever shrinks via on-write rotation, so a sweep can
/// never drop audit records. Returns the number of generation files
/// removed.
///
/// #3671: each store is swept under its own companion lock
/// (non-blocking — a contended store is SKIPPED, never waited on, so tick
/// cadence never stalls behind a writer). Writer rotation is a multi-step
/// remove/shift/rename sequence; a lock-free sweep could observe `.4` as
/// over-cap after the writer removed `.5` but before the `.4`→`.5` rename
/// landed, delete `.4`, and the writer's ignored rename error would then
/// drop that generation permanently.
///
/// Covered stores and their policies live in the module retention table.
pub fn sweep_rotated_generations(home: &Path) -> usize {
    let stores: &[(&str, usize, Duration)] = &[
        ("fleet_events.jsonl", 5, Duration::from_secs(30 * 86400)),
        (
            "state-transitions.jsonl",
            5,
            Duration::from_secs(30 * 86400),
        ),
    ];
    let now = SystemTime::now();
    let mut removed = 0;
    for (file, max_gens, max_age) in stores {
        let path = home.join(file);
        // Non-blocking: skip the store when a writer holds its companion
        // lock rather than pruning under an in-flight rotation. The guard
        // must stay alive for the whole prune — dropping it early would
        // reopen the interleave window.
        let _guard = match crate::store::try_acquire_file_lock(&path.with_extension("jsonl.lock")) {
            Ok(Some(guard)) => guard,
            Ok(None) | Err(_) => continue,
        };
        let before: Vec<PathBuf> = (1..=(*max_gens + 4))
            .map(|gen| rotated_path(&path, gen))
            .filter(|p| p.exists())
            .collect();
        prune_rotated(
            &path,
            RetentionPolicy {
                max_live_bytes: u64::MAX,
                max_rotated_files: *max_gens,
                max_rotated_age: *max_age,
            },
            now,
        );
        removed += before.into_iter().filter(|p| !p.exists()).count();
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-jsonl-retention-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn policy() -> RetentionPolicy {
        RetentionPolicy {
            max_live_bytes: 64,
            max_rotated_files: 2,
            max_rotated_age: Duration::from_secs(30 * 86400),
        }
    }

    /// An already-oversized live file must rotate down on the very next
    /// write: the old content moves to `.1`, the live file holds only the
    /// new record.
    #[test]
    fn oversized_live_rotates_on_next_write_3669() {
        let home = tmp_home("oversize");
        let path = home.join("state-transitions.jsonl");
        std::fs::write(&path, format!("old:{}\n", "x".repeat(200))).unwrap();

        append_line_with_retention(&path, &json!({"ts": "now"}), policy()).unwrap();

        let live = std::fs::metadata(&path).unwrap().len();
        assert!(
            live <= policy().max_live_bytes,
            "live file must rotate down on next write; got {live}"
        );
        assert!(
            rotated_path(&path, 1).exists(),
            "rotation must preserve the old content in generation .1"
        );
        let body = std::fs::read_to_string(rotated_path(&path, 1)).unwrap();
        assert!(
            body.contains("\"ts\""),
            "the crossing record must be preserved in generation .1 (usage_stats pattern: append-then-rotate): {body}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Generations past the cap are pruned: with `.1` and `.2` seeded and
    /// `max_rotated_files = 2`, a further rotation must not leave a `.3`.
    #[test]
    fn generation_cap_prunes_oldest_3669() {
        let home = tmp_home("gens");
        let path = home.join("state-transitions.jsonl");
        std::fs::write(rotated_path(&path, 1), "gen-1\n").unwrap();
        std::fs::write(rotated_path(&path, 2), "gen-2\n").unwrap();
        std::fs::write(&path, format!("live:{}\n", "x".repeat(200))).unwrap();

        append_line_with_retention(&path, &json!({"ts": "now"}), policy()).unwrap();

        let live = std::fs::metadata(&path).unwrap().len();
        assert!(
            live <= policy().max_live_bytes,
            "live file must rotate down on next write; got {live}"
        );
        assert!(
            !rotated_path(&path, 3).exists(),
            "generation cap must prune beyond max_rotated_files"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A stale rotated generation (mtime older than the age cap) is pruned
    /// even when no size rotation fires.
    #[test]
    fn age_cap_prunes_stale_generations_3669() {
        let home = tmp_home("age");
        let path = home.join("state-transitions.jsonl");
        let old = rotated_path(&path, 1);
        let fresh = rotated_path(&path, 2);
        std::fs::write(&old, "old\n").unwrap();
        std::fs::write(&fresh, "fresh\n").unwrap();

        let stale = SystemTime::now() - Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(stale)
            .unwrap();

        let age_policy = RetentionPolicy {
            max_live_bytes: 1024 * 1024,
            max_rotated_files: 5,
            max_rotated_age: Duration::from_secs(1),
        };
        append_line_with_retention(&path, &json!({"ts": "now"}), age_policy).unwrap();

        assert!(!old.exists(), "stale rotated generation must be pruned");
        assert!(fresh.exists(), "fresh rotated generation must be retained");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3671-B RED: the hourly sweep must serialize with the writer's
    /// companion lock. Writer rotation is a multi-step remove/shift/rename
    /// sequence; a lock-free sweep can observe `.4` as over-cap after the
    /// writer removed `.5` but before the `.4`→`.5` rename lands, delete
    /// `.4`, and the writer's ignored rename error then drops that
    /// generation permanently. While a writer holds the companion lock the
    /// sweep must skip that store (non-blocking — tick cadence must not
    /// stall) instead of pruning under it.
    #[test]
    fn sweep_skips_store_while_writer_holds_lock_3671() {
        let home = tmp_home("sweep-locked-3671");
        let path = home.join("state-transitions.jsonl");
        std::fs::write(&path, "live\n").unwrap();
        let residue = rotated_path(&path, 6);
        std::fs::write(&residue, "gen\n").unwrap();

        let lock_path = path.with_extension("jsonl.lock");
        let _guard =
            crate::store::acquire_file_lock(&lock_path).expect("test holds the writer lock");

        let _swept = sweep_rotated_generations(&home);

        assert!(
            residue.exists(),
            "#3671-B: sweep must not prune a store whose companion lock is \
             held by a writer — rotation is a multi-step remove/rename \
             sequence and an interleaved sweep can drop a generation \
             permanently"
        );

        drop(_guard);
        let swept_after = sweep_rotated_generations(&home);
        assert!(
            swept_after >= 1 && !residue.exists(),
            "after the writer releases the lock the sweep must clear the \
             over-cap generation; got swept={swept_after}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3671-D: the age cap boundary is strict-greater-than — a generation
    /// exactly at the cap is retained, one past it is pruned. (±60 s margins
    /// keep filesystem mtime granularity out of the assertion.)
    #[test]
    fn age_cap_boundary_is_strict_greater_than_3671() {
        let home = tmp_home("age-boundary-3671");
        let path = home.join("state-transitions.jsonl");
        let at_cap = rotated_path(&path, 1);
        let past_cap = rotated_path(&path, 2);
        std::fs::write(&at_cap, "at-cap\n").unwrap();
        std::fs::write(&past_cap, "past-cap\n").unwrap();

        let age_policy = RetentionPolicy {
            max_live_bytes: 1024 * 1024,
            max_rotated_files: 5,
            max_rotated_age: Duration::from_secs(3600),
        };
        let now = SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&at_cap)
            .unwrap()
            .set_modified(now - Duration::from_secs(3540))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&past_cap)
            .unwrap()
            .set_modified(now - Duration::from_secs(3660))
            .unwrap();

        append_line_with_retention(&path, &json!({"ts": "now"}), age_policy).unwrap();

        assert!(
            at_cap.exists(),
            "a generation within the age cap must be retained"
        );
        assert!(
            !past_cap.exists(),
            "a generation past the age cap must be pruned"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3671-D: a symlinked generation is removed as a link — the sweep
    /// deletes the `.N` link itself and never follows it into the target.
    #[test]
    #[cfg(unix)]
    fn sweep_removes_symlink_generation_but_keeps_target_3671() {
        let home = tmp_home("symlink-3671");
        let path = home.join("state-transitions.jsonl");
        std::fs::write(&path, "live\n").unwrap();
        let target = home.join("outside-target.txt");
        std::fs::write(&target, "outside\n").unwrap();
        let link = rotated_path(&path, 6);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let swept = sweep_rotated_generations(&home);

        assert!(
            swept >= 1 && !link.exists() && target.exists(),
            "sweep must remove the over-cap symlink generation itself while \
             keeping its target; swept={swept}"
        );
        assert!(
            std::fs::read_to_string(&path).unwrap() == "live\n",
            "the live file must be byte-identical after a sweep"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3671-D: concurrent writers (rotating under the companion lock) and
    /// the hourly sweep interleave without loss or panic — every acknowledged
    /// row survives somewhere in live + generations, and the live file only
    /// ever holds complete lines.
    #[test]
    fn concurrent_writers_and_sweep_lose_nothing_3671() {
        use std::sync::{Arc, Barrier};
        let home = tmp_home("concurrent-3671");
        let path = home.join("state-transitions.jsonl");
        let tiny = RetentionPolicy {
            max_live_bytes: 512,
            max_rotated_files: 3,
            max_rotated_age: Duration::from_secs(30 * 86400),
        };
        const WRITERS: usize = 4;
        const ROWS: usize = 25;
        let barrier = Arc::new(Barrier::new(WRITERS + 1));
        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let home = home.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for r in 0..ROWS {
                    let row = json!({"w": w, "r": r});
                    append_line_with_retention(&path, &row, tiny).unwrap();
                    if r % 5 == 0 {
                        sweep_rotated_generations(&home);
                    }
                }
            }));
        }
        barrier.wait();
        for h in handles {
            h.join().expect("writer thread must not panic");
        }
        sweep_rotated_generations(&home);

        let mut seen = std::collections::HashSet::new();
        let mut files = vec![path.clone()];
        for gen in 1..=tiny.max_rotated_files {
            let p = rotated_path(&path, gen);
            if p.exists() {
                files.push(p);
            }
        }
        for f in &files {
            for line in std::fs::read_to_string(f).unwrap().lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let v: serde_json::Value =
                    serde_json::from_str(line).expect("every row must be parseable");
                seen.insert((v["w"].as_u64().unwrap(), v["r"].as_u64().unwrap()));
            }
        }
        assert_eq!(
            seen.len(),
            WRITERS * ROWS,
            "every acknowledged row must survive in live + generations"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The hourly sweep removes over-cap generations but NEVER the live
    /// file: residue left by an older build (`.6`, `.7`) is swept while
    /// `state-transitions.jsonl` itself is untouched.
    #[test]
    fn sweep_removes_residue_but_never_live_3669() {
        let home = tmp_home("sweep");
        let path = home.join("state-transitions.jsonl");
        std::fs::write(&path, "live\n").unwrap();
        for gen in 1..=7 {
            std::fs::write(rotated_path(&path, gen), "gen\n").unwrap();
        }

        let swept = sweep_rotated_generations(&home);

        assert!(
            swept >= 2,
            "over-cap generations must be swept; got {swept}"
        );
        assert!(
            std::fs::read_to_string(&path).unwrap() == "live\n",
            "the live file must be byte-identical after a sweep"
        );
        assert!(
            !rotated_path(&path, 6).exists() && !rotated_path(&path, 7).exists(),
            "generations past the cap must be gone"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
