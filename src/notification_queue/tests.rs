#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

fn tmp_home(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-notification-queue-{}-{}",
        suffix,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn hold_metadata_lock(home: &Path) -> (crossbeam_channel::Sender<()>, std::thread::JoinHandle<()>) {
    let lock_path = agent_ops::metadata_path_resolved(home, "agent1").with_extension("lock");
    let (locked_tx, locked_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let holder = std::thread::spawn(move || {
        let guard = crate::store::acquire_file_lock(&lock_path).unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        drop(guard);
    });
    locked_rx.recv().unwrap();
    (release_tx, holder)
}

/// Retry-accumulate `drain` to absorb #2028's transient `Unavailable→empty`.
/// `drain()` is contractually allowed to return an empty vec when
/// `try_acquire_file_lock` hits a transient open/lock hiccup under heavy
/// parallel load (llvm-cov-grade fd pressure) — production's flusher simply
/// retries next tick, so a one-shot drain is NOT authoritative. A test that
/// trusts it indexes an empty vec → index panic (the #2072 coverage flake,
/// notification_queue.rs `again[0]`). This helper models the retry: it keeps
/// draining (accumulating, since `drain` is destructive) until it has `want`
/// items or the bounded budget elapses. The happy path returns on the first
/// attempt — zero behavior change when the drain succeeds immediately.
fn drain_settled(home: &Path, agent_name: &str, want: usize) -> Vec<QueuedNotification> {
    drain_settled_with_stale(home, agent_name, want, STALE_DRAINING_MS)
}

fn drain_settled_with_stale(
    home: &Path,
    agent_name: &str,
    want: usize,
    stale_ms: u128,
) -> Vec<QueuedNotification> {
    let mut acc = Vec::new();
    for _ in 0..200 {
        acc.extend(drain_with_stale_threshold(home, agent_name, stale_ms));
        if acc.len() >= want {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    acc
}

// #t-3558 P2 — coalesce keep-latest, asserted as a NO-LOSS invariant.
//
// Pins (lead's hard conditions): the rewrite drops ONLY same-kind AGEND-AUTO
// lines, never a normal message nor a different AGEND-AUTO kind.
//
// The EXACT keep-latest collapse (rl#2 drops rl#1 → one ratelimit line) is NOT
// asserted here. `enqueue_coalesced_auto` legitimately falls back to a no-loss
// plain append whenever it can't grab the per-agent `drain.lock`
// (`enqueue_coalesced_auto`, the `try_acquire_file_lock` gate). Under the
// Coverage job's llvm-cov `cargo test` (heavy parallel, non-nextest) fd
// pressure makes that lock's `.open()` transiently EMFILE → `Err` → the
// fallback fires even with NO real contender, leaving rl#1 un-coalesced → 4
// lines instead of 3. That is correct by design — every nudge is preserved, no
// message is lost — so an exact-count assertion was a false flake. #2333
// relaxed the sibling `coalesce_preserves_unparseable_lines` for this IDENTICAL
// mechanism but missed this test (same family as #2028/#2072/#2074).
//
// The collapse itself stays deterministically covered by
// `coalesce_falls_back_to_append_when_drain_lock_held_no_loss` (forces the
// lock-free path → asserts exactly one line), so a refactor that breaks
// coalescing is still caught under the no-fd-pressure (nextest) gate.
#[test]
fn coalesce_keeps_latest_same_kind_and_preserves_others() {
    let home = tmp_home("coalesce-keep");
    let a = "agent";
    let rl = "[AGEND-AUTO kind=ratelimit-retry] continue";
    let pb = "[AGEND-AUTO kind=progress-backstop] continue";
    enqueue(&home, a, "hello world").expect("normal");
    enqueue_coalesced_auto(&home, a, rl).expect("rl#1");
    enqueue_coalesced_auto(&home, a, pb).expect("different kind");
    enqueue_coalesced_auto(&home, a, rl).expect("rl#2 coalesces rl#1");

    // Read the queue file directly — like the sibling
    // `coalesce_preserves_unparseable_lines` — instead of draining. The
    // coalesce writes the queue, so a direct read asserts the coalesce outcome
    // without dragging in the SEPARATE #2028/#2072 drain-transient surface
    // (`drain`'s own lock `.open()` EMFILEs under the same fd pressure and can
    // under-deliver, which would re-introduce a different flake here).
    let raw = std::fs::read_to_string(queue_path(&home, a)).expect("queue file");
    let lines: Vec<&str> = raw.lines().collect();
    // No loss, bounded: 3 = coalesced (rl#1 dropped), 4 = fd-pressure fallback
    // append (rl#1 kept). Both preserve every message; neither runs away.
    assert!(
        (3..=4).contains(&lines.len()),
        "no-loss bounded total (3=coalesced, 4=fallback append); got:\n{raw}"
    );
    let rl_kept = lines
        .iter()
        .filter(|l| l.contains("kind=ratelimit-retry"))
        .count();
    // 1 = coalesced, 2 = fd-pressure fallback append — the LATEST ratelimit
    // nudge is always present and never duplicated beyond the single fallback.
    assert!(
        (1..=2).contains(&rl_kept),
        "latest ratelimit nudge preserved (1=coalesced, 2=fallback); got:\n{raw}"
    );
    assert!(
        lines.iter().any(|l| l.contains("hello world")),
        "normal message preserved; got:\n{raw}"
    );
    assert!(
        lines.iter().any(|l| l.contains("kind=progress-backstop")),
        "a DIFFERENT AGEND-AUTO kind is never coalesced away; got:\n{raw}"
    );
}

// #t-3558 P2 — coalesce operates on RAW lines, so a non-JSON/unparseable
// queue line is preserved byte-for-byte (never dropped by the filter rewrite).
#[test]
fn coalesce_preserves_unparseable_lines() {
    let home = tmp_home("coalesce-raw");
    let a = "agent";
    let rl = "[AGEND-AUTO kind=ratelimit-retry] continue";
    let qp = queue_path(&home, a);
    std::fs::create_dir_all(qp.parent().unwrap()).unwrap();
    {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&qp)
            .unwrap();
        writeln!(f, "GARBAGE not json").unwrap();
    }
    enqueue_coalesced_auto(&home, a, rl).expect("rl#1");
    enqueue_coalesced_auto(&home, a, rl).expect("rl#2 coalesces");

    let raw = std::fs::read_to_string(&qp).unwrap();
    assert!(
        raw.lines().any(|l| l == "GARBAGE not json"),
        "unparseable line preserved; got:\n{raw}"
    );
    // The ratelimit nudge is never LOST, whether this enqueue hit the coalesce
    // path (drain lock acquired → 1 line, keep-latest) or the contention
    // fallback (lock momentarily held → plain append → 2 lines). BOTH are
    // no-loss. coverage (llvm-cov) instrumentation perturbs scheduling and can
    // flip the lock race, so assert the no-loss invariant (latest present, no
    // runaway duplication) — NOT a single path — to keep this test about its
    // actual subject: the unparseable line surviving the rewrite. The
    // coalesce-vs-fallback paths themselves are pinned deterministically by
    // `coalesce_falls_back_to_append_when_drain_lock_held_no_loss`.
    let rl_count = raw
        .lines()
        .filter(|l| l.contains("kind=ratelimit-retry"))
        .count();
    assert!(
        (1..=2).contains(&rl_count),
        "ratelimit nudge preserved (1=coalesced, 2=fallback append; both no-loss); got:\n{raw}"
    );
}

// #t-3558 P2 — drain-lock contention: when a drainer holds the lock, coalesce
// SKIPS (no read-modify-write) and falls back to a plain append → no message
// loss while contended; coalesce resumes once the lock frees.
#[test]
fn coalesce_falls_back_to_append_when_drain_lock_held_no_loss() {
    let home = tmp_home("coalesce-fallback");
    let a = "agent";
    let rl = "[AGEND-AUTO kind=ratelimit-retry] continue";
    enqueue_coalesced_auto(&home, a, rl).expect("rl#1");
    {
        // Simulate a drainer mid-claim by holding the per-agent drain lock.
        // #2666 uncovered facet: acquire via the retrying `acquire_drain_lock`,
        // not a raw acquire that flakes on a transient Err under llvm-cov.
        let _held = acquire_drain_lock(&drain_lock_path(&home, a))
            .expect("lock op")
            .expect("lock acquired");
        enqueue_coalesced_auto(&home, a, rl).expect("rl#2 under held lock → fallback append");
        let raw = std::fs::read_to_string(queue_path(&home, a)).unwrap();
        assert_eq!(
            raw.lines().count(),
            2,
            "lock held → fallback append keeps BOTH nudges (no loss); got:\n{raw}"
        );
    }
    // Lock freed → a fresh coalesce now collapses to keep-latest.
    enqueue_coalesced_auto(&home, a, rl).expect("rl#3 coalesces once free");
    let drained = drain_settled(&home, a, 1);
    assert_eq!(
        drained.len(),
        1,
        "after the lock frees, coalesce keeps exactly the latest"
    );
}

#[test]
fn enqueue_classified_round_trips_actionable_and_deferred_since_1513() {
    let home = tmp_home("classified");
    enqueue_classified(&home, "a", "work", true).expect("enqueue actionable");
    enqueue(&home, "a", "ambient").expect("enqueue ambient"); // actionable=false default
    let drained = drain_settled(&home, "a", 2);
    assert_eq!(drained.len(), 2);
    let actionable = drained
        .iter()
        .find(|q| q.text == "work")
        .expect("find actionable");
    assert!(
        actionable.actionable,
        "actionable flag preserved across serde"
    );
    assert!(actionable.deferred_since_ms > 0, "deferred_since stamped");
    let ambient = drained
        .iter()
        .find(|q| q.text == "ambient")
        .expect("find ambient");
    assert!(!ambient.actionable, "ambient default false");
    // requeue preserves the original deferred_since (cap counts from first defer)
    let since = actionable.deferred_since_ms;
    requeue_all(&home, "a", std::slice::from_ref(actionable));
    let again = drain_settled(&home, "a", 1);
    assert_eq!(
        again[0].deferred_since_ms, since,
        "requeue preserves deferred_since"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn drain_settled_retries_through_transient_lock_contention_2072() {
    // Deterministic reproduction of the #2072 coverage-flake MECHANISM:
    // while the drain lock reads as held, a one-shot `drain()` returns empty
    // (#2028 `Unavailable→empty`), so a test that trusts it indexes an empty
    // vec → index panic (the live failure at `again[0]`). `drain_settled`
    // must RETRY across the contention window and recover the item once the
    // lock frees — exactly what the production flusher does next tick. The
    // contention is injected via the path-keyed `force_contention` seam (no
    // peer thread, no timing window), so it's immune to llvm-cov timing.
    let home = tmp_home("transient_lock_2072");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    enqueue(&home, "a", "delayed").expect("enqueue");
    let lp = drain_lock_path(&home, "a");

    test_hooks::arm_contention(&lp);
    // A one-shot drain while the lock reads held is empty — the exact trap.
    assert!(
        drain(&home, "a").is_empty(),
        "one-shot drain under contention returns empty (#2028 Unavailable→empty)"
    );
    test_hooks::clear_contention(&lp);

    let got = drain_settled(&home, "a", 1);
    assert_eq!(
        got.len(),
        1,
        "drain_settled recovers the item after the lock frees"
    );
    assert_eq!(got[0].text, "delayed");
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn pending_count_tracks_enqueued_notifications() {
    let home = tmp_home("count");
    enqueue(&home, "agent1", "a").expect("enqueue a");
    enqueue(&home, "agent1", "b").expect("enqueue b");
    assert_eq!(pending_count(&home, "agent1"), 2);
    std::fs::remove_dir_all(home).ok();
}

// ── #2967/#2978/#2979: QueueDirSnapshot — preservation ──
//
// These must pass BOTH before AND after the snapshot-consumer rewire
// (they exercise `pending_count`/`QueueDirSnapshot` directly, not the
// rewired call sites) — they pin that the new one-`read_dir` accessors
// agree with the pre-existing per-call `pending_count` in every shape
// that function ever handled.

#[test]
fn snapshot_agrees_with_pending_count_empty_dir_2978() {
    let home = tmp_home("snap-empty-dir");
    std::fs::remove_dir_all(&home).ok();
    // No notification-queue/ dir created at all.
    let snap = QueueDirSnapshot::scan(&home);
    assert_eq!(pending_count(&home, "agent1"), 0);
    assert_eq!(snap.pending_count("agent1"), 0);
    assert!(!snap.has_pending("agent1"));
}

#[test]
fn snapshot_agrees_with_pending_count_empty_file_2978() {
    let home = tmp_home("snap-empty-file");
    std::fs::remove_dir_all(&home).ok();
    let path = queue_path(&home, "agent1");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"").unwrap(); // zero-length queue file
    let snap = QueueDirSnapshot::scan(&home);
    assert_eq!(pending_count(&home, "agent1"), 0);
    assert_eq!(snap.pending_count("agent1"), 0);
    assert!(
        !snap.has_pending("agent1"),
        "a zero-length file must not read as pending"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn snapshot_agrees_with_pending_count_one_item_2978() {
    let home = tmp_home("snap-one");
    std::fs::remove_dir_all(&home).ok();
    enqueue(&home, "agent1", "a").expect("enqueue");
    let snap = QueueDirSnapshot::scan(&home);
    assert_eq!(pending_count(&home, "agent1"), 1);
    assert_eq!(snap.pending_count("agent1"), 1);
    assert!(snap.has_pending("agent1"));
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn snapshot_agrees_with_pending_count_ten_items_2978() {
    let home = tmp_home("snap-ten");
    std::fs::remove_dir_all(&home).ok();
    for i in 0..10 {
        enqueue(&home, "agent1", &format!("msg-{i}")).expect("enqueue");
    }
    let snap = QueueDirSnapshot::scan(&home);
    assert_eq!(pending_count(&home, "agent1"), 10);
    assert_eq!(snap.pending_count("agent1"), 10);
    assert!(snap.has_pending("agent1"));
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn snapshot_agrees_with_pending_count_stale_draining_leftover_2978() {
    let home = tmp_home("snap-stale-draining");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    enqueue(&home, "agent1", "crashed-claim").expect("enqueue");
    std::fs::rename(queue_path(&home, "agent1"), draining_path(&home, "agent1"))
        .expect("simulate crashed drainer's leftover claim");
    let snap = QueueDirSnapshot::scan(&home);
    assert_eq!(
        pending_count(&home, "agent1"),
        1,
        "a leftover draining file still counts as pending"
    );
    assert_eq!(snap.pending_count("agent1"), 1);
    assert!(snap.has_pending("agent1"));
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn snapshot_agrees_with_pending_count_ignores_drain_lock_2978() {
    let home = tmp_home("snap-drain-lock");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    // A drain.lock with NO queue/draining content — the #1944-adjacent
    // rule `list_draining_files`' doc comment protects: the lock file
    // must never be mistaken for queue content.
    std::fs::create_dir_all(home.join("notification-queue")).unwrap();
    std::fs::write(drain_lock_path(&home, "agent1"), b"lock").unwrap();
    let snap = QueueDirSnapshot::scan(&home);
    assert_eq!(
        pending_count(&home, "agent1"),
        0,
        "a bare .drain.lock file must never be counted as queue content"
    );
    assert_eq!(snap.pending_count("agent1"), 0);
    assert!(!snap.has_pending("agent1"));
    std::fs::remove_dir_all(home).ok();
}

/// #2979: no message loss — a concurrent enqueue landing AFTER the
/// snapshot is taken must still be delivered by `drain` in the same pass
/// (the snapshot only decides whether to bother LOOKING; the actual
/// delivery path — `drain`/`try_drain_with_stale_threshold` — always
/// claims and reads the LIVE queue file, completely independent of what
/// the snapshot saw). This is the untouched-by-design half of the
/// contract: the snapshot has no bearing on `drain`'s correctness because
/// `drain` never consults it. A stale-at-scan-time snapshot (`has_pending`
/// false at scan) followed by an enqueue must still see that item
/// delivered — proving a productive agent is never skipped just because
/// the gate ran before the enqueue.
#[test]
fn snapshot_taken_then_concurrent_enqueue_is_not_lost_2979() {
    let home = tmp_home("snap-concurrent-enqueue");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    let snap = QueueDirSnapshot::scan(&home); // taken while queue is empty
    assert!(!snap.has_pending("agent1"), "empty at scan time");
    // Enqueue AFTER the snapshot was taken. `drain` never consults the
    // snapshot, so once a drain runs the late arrival is delivered — the
    // snapshot cannot hide it from the claim protocol. (Whether THIS pass
    // reaches the drain is a separate question, pinned honestly by
    // `enqueue_after_scan_is_invisible_to_that_pass_2979` below.)
    enqueue(&home, "agent1", "late-arrival").expect("enqueue after snapshot");
    let drained = drain_settled(&home, "agent1", 1);
    assert_eq!(drained.len(), 1, "the late arrival is drained, not lost");
    assert_eq!(drained[0].text, "late-arrival");
    std::fs::remove_dir_all(home).ok();
}

/// The ONE behavioural consequence of gating a pass on a per-pass
/// snapshot, pinned deliberately rather than left for someone to discover:
/// an item enqueued AFTER a pass took its snapshot is invisible to THAT
/// pass's gate, so its delivery moves to the next pass. Bounded by exactly
/// one flush interval and never lost — and the pre-snapshot code was
/// already arbitrary here, since `pending_count` ran at each agent's turn
/// in the fleet iteration, so whether a mid-pass arrival was seen depended
/// on where that agent happened to sit in the loop.
#[test]
fn enqueue_after_scan_is_invisible_to_that_pass_2979() {
    let home = tmp_home("snap-late-arrival-gate");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();

    let this_pass = QueueDirSnapshot::scan(&home);
    enqueue(&home, "agent1", "late-arrival").expect("enqueue after snapshot");

    assert!(
        !this_pass.has_pending("agent1"),
        "a pass gates on ITS OWN snapshot: an arrival after the scan is not \
         visible to that pass"
    );
    let next_pass = QueueDirSnapshot::scan(&home);
    assert!(
        next_pass.has_pending("agent1"),
        "the next pass's snapshot sees it — the delay is bounded by one \
         flush interval, and nothing is lost"
    );
    std::fs::remove_dir_all(home).ok();
}

/// Single-drainer ordering under the new snapshot-gated call sites is
/// unaffected — `QueueDirSnapshot` never touches `drain`'s claim/rename
/// protocol, so the existing `concurrent_drains_deliver_exactly_once`
/// (above) IS this preservation test; nothing new to add here beyond
/// re-asserting the invariant it already pins stays untouched by this
/// change (the snapshot has no write path and never claims a file).
#[test]
fn snapshot_has_no_write_path_2979() {
    let home = tmp_home("snap-read-only");
    std::fs::remove_dir_all(&home).ok();
    enqueue(&home, "agent1", "a").expect("enqueue");
    let before = std::fs::read_to_string(queue_path(&home, "agent1")).unwrap();
    let _snap = QueueDirSnapshot::scan(&home);
    let _ = _snap.pending_count("agent1");
    let _ = _snap.has_pending("agent1");
    let after = std::fs::read_to_string(queue_path(&home, "agent1")).unwrap();
    assert_eq!(
        before, after,
        "scanning/reading a snapshot must not mutate the queue file"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn drain_roundtrip() {
    let home = tmp_home("drain");
    enqueue(&home, "agent1", "a").expect("enqueue a");
    enqueue(&home, "agent1", "b").expect("enqueue b");
    let drained = drain_settled(&home, "agent1", 2);
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].text, "a");
    assert_eq!(pending_count(&home, "agent1"), 0);
    std::fs::remove_dir_all(home).ok();
}

/// Sprint 54 P2-3: round-trip both timestamps; ensure
/// `read_input_submit_timestamps` returns paired values and
/// `record_submit_activity` records a value strictly newer than the
/// preceding `record_input_activity` call and the pair flushes together.
#[test]
fn record_and_read_input_submit_timestamps_round_trip() {
    let home = tmp_home("ts_round_trip");
    // Fresh agent → both 0.
    let (typed0, submit0) = read_input_submit_timestamps(&home, "agent1");
    assert_eq!((typed0, submit0), (0, 0));
    record_input_activity(&home, "agent1");
    flush_pending_input_activity(&home);
    std::thread::sleep(Duration::from_millis(2));
    record_submit_activity(&home, "agent1");
    flush_pending_input_activity(&home);
    let (typed1, submit1) = read_input_submit_timestamps(&home, "agent1");
    assert!(typed1 > 0, "typed timestamp must be set after record");
    assert!(submit1 > 0, "submit timestamp must be set after record");
    assert!(
        submit1 >= typed1,
        "submit (called second) must be ≥ typed (called first), got typed={typed1} submit={submit1}"
    );
    std::fs::remove_dir_all(home).ok();
}

/// The operator's Enter keystroke must never pin the single-threaded TUI
/// event loop. `app::write_to_focused` calls `record_submit_activity`
/// INLINE on that loop whenever the keystroke buffer contains the backend's
/// submit key (`\r` for every preset since #1457). That call lands in
/// `save_metadata` → `with_json_state_or_create` → `acquire_file_lock`,
/// whose `fs4::FileExt::lock` is a BLOCKING flock with no timeout.
///
/// The same per-instance `metadata/<id>.lock` is taken by ~29 other write
/// sites, including `flush_pending_input_activity` on the ~1s `sync_badges`
/// cadence — which writes the file of the very agent being typed to. When
/// Enter races that flush, the whole TUI stalls: no tab/pane switching, no
/// render, while the agents (separate processes) keep running.
///
/// Contrast `record_input_activity`, which #2965 already moved off the
/// synchronous path into an in-memory buffer. That fix skipped the submit
/// twin, which is why plain typing is smooth and only Enter freezes.
/// #3321 is FIXED in main: `record_submit_activity` now delegates to the shared
/// buffered `record_activity`, and the only blocking flush moved to teardown. This
/// was the reproduction; it is now the ACCEPTANCE test and runs by default. Keep it
/// enabled — it is what catches a regression back onto the synchronous flock.
#[test]
fn record_submit_activity_must_not_block_on_contended_metadata_lock() {
    let home = tmp_home("submit_flock_contention");
    std::fs::create_dir_all(home.join("metadata")).ok();
    let lock_path =
        crate::agent_ops::metadata_path_resolved(&home, "agent1").with_extension("lock");

    // Stand in for any of the other metadata writers holding the lock.
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let hold_for = Duration::from_millis(1500);
    let lp = lock_path.clone();
    let holder = std::thread::spawn(move || {
        let guard = crate::store::acquire_file_lock(&lp).expect("holder acquires lock");
        held_tx.send(()).expect("signal lock held");
        std::thread::sleep(hold_for);
        drop(guard);
    });
    held_rx.recv().expect("holder signalled");

    // The TUI main-loop path.
    let start = std::time::Instant::now();
    record_submit_activity(&home, "agent1");
    let elapsed = start.elapsed();

    holder.join().expect("holder thread");
    std::fs::remove_dir_all(&home).ok();

    assert!(
        elapsed < Duration::from_millis(300),
        "record_submit_activity blocked {elapsed:?} on a contended metadata \
         flock while the lock was held for {hold_for:?}. On the TUI thread \
         this is a full UI freeze on every Enter that races another writer."
    );
}

/// Control for the test above: the SAME contended lock, the SAME code path
/// up to the recording call — but plain typing goes through
/// `record_input_activity`, which #2965 moved into an in-memory buffer. It
/// must return immediately. The delta between these two tests is exactly
/// the operator-visible symptom: typing stays smooth, Enter freezes.
#[test]
fn record_input_activity_does_not_block_on_contended_metadata_lock() {
    let home = tmp_home("input_flock_contention");
    std::fs::create_dir_all(home.join("metadata")).ok();
    let lock_path =
        crate::agent_ops::metadata_path_resolved(&home, "agent1").with_extension("lock");

    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let hold_for = Duration::from_millis(1500);
    let lp = lock_path.clone();
    let holder = std::thread::spawn(move || {
        let guard = crate::store::acquire_file_lock(&lp).expect("holder acquires lock");
        held_tx.send(()).expect("signal lock held");
        std::thread::sleep(hold_for);
        drop(guard);
    });
    held_rx.recv().expect("holder signalled");

    let start = std::time::Instant::now();
    record_input_activity(&home, "agent1");
    let elapsed = start.elapsed();

    holder.join().expect("holder thread");
    std::fs::remove_dir_all(&home).ok();

    assert!(
        elapsed < Duration::from_millis(300),
        "record_input_activity must stay off the flock (it buffers in memory \
         since #2965), but it took {elapsed:?}"
    );
}

/// #1680 regression: the keystroke WRITE (`record_input_activity` →
/// `save_metadata` → `metadata_path_resolved` → `<uuid>.json`) and the
/// draft-gate READ (`read_input_submit_timestamps`) MUST land on the SAME
/// file when fleet.yaml maps the name → a UUID. Pre-#1680 the read hand-coded
/// `<name>.json` (bypassing the resolver) while the write went to
/// `<uuid>.json`, so they never intersected → `draft_state` was permanently
/// stale (`None`) → the inject path force-submitted the operator's unsent
/// draft. Every prior test used a home with NO fleet.yaml (the resolver falls
/// back to the name path, so write/read happen to converge) and so could not
/// catch the split. This pins the id-mapped path: it FAILS before the read
/// resolver alignment and PASSES after.
#[test]
fn draft_gate_read_resolves_uuid_like_write_1680() {
    let home = tmp_home("draft_gate_uuid_1680");
    // Isolate from any prior run sharing the same (suffix,pid) dir.
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    // fleet.yaml maps the fleet NAME → a UUID, so the resolver routes
    // metadata to `<uuid>.json` — the production shape that exposed the split.
    let id = crate::types::InstanceId::new();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  fixup-x:\n    id: {}\n", id.full()),
    )
    .expect("write fleet.yaml");

    // WRITE a compose keystroke (no submit) → `<uuid>.json` via the resolver.
    record_input_activity(&home, "fixup-x");
    flush_pending_input_activity(&home);

    // READ must see it through the SAME resolver. Pre-fix this reads the
    // never-written `<name>.json` and returns 0 → assert fails (RED).
    let (typed, submit) = read_input_submit_timestamps(&home, "fixup-x");
    assert!(
        typed > 0,
        "#1680: read must resolve the same UUID file the write used \
         (got typed=0 → it read the stale name-path)"
    );
    assert_eq!(submit, 0, "no submit recorded yet");
    // End-to-end gate signal: a recent unsent draft must read as Drafting,
    // NOT None — `None` is precisely what let the inject clobber the draft.
    assert_eq!(
        draft_state(&home, "fixup-x"),
        DraftState::Drafting,
        "#1680: a live unsent operator draft must gate (Drafting), not read as None"
    );
    std::fs::remove_dir_all(home).ok();
}

/// Sprint 54 P2-3: typed-only (no submit) must read as
/// `submit_ms == 0`. This is the daemon-supervisor's signal for
/// "user typed but never pressed Enter" — it MUST distinguish
/// from "user typed AND submitted" otherwise the dedup logic
/// degrades to never firing.
#[test]
fn typed_only_leaves_submit_zero() {
    let home = tmp_home("typed_only");
    record_input_activity(&home, "agent1");
    flush_pending_input_activity(&home);
    let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
    assert!(typed > 0);
    assert_eq!(
        submit, 0,
        "submit must stay 0 until record_submit_activity is called"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #1457: write raw input/submit timestamps so draft-state tests are
/// deterministic (no sleeps / no process-global env).
fn write_ts(home: &Path, agent: &str, typed_ms: i64, submit_ms: i64) {
    if typed_ms != 0 {
        agent_ops::save_metadata(home, agent, COMPOSE_METADATA_KEY, json!(typed_ms));
    }
    if submit_ms != 0 {
        agent_ops::save_metadata(home, agent, SUBMIT_METADATA_KEY, json!(submit_ms));
    }
}

// ── #1944: input_box_is_empty (buffer-content draft refinement) ──

#[test]
fn input_box_empty_when_only_prompt_marker() {
    // claude `❯ ` with nothing typed (real capture: claude-discussion-text.raw
    // ends in exactly this) → empty.
    assert_eq!(input_box_is_empty("some output\n❯ ", "❯"), Some(true));
    assert_eq!(input_box_is_empty("output\n> ", ">"), Some(true));
    // trailing whitespace / wrapped blank only
    assert_eq!(input_box_is_empty("❯   \n", "❯"), Some(true));
}

#[test]
fn input_box_nonempty_when_text_after_marker() {
    // a real live draft → protect (defer).
    assert_eq!(
        input_box_is_empty("output\n❯ hello world", "❯"),
        Some(false)
    );
    assert_eq!(input_box_is_empty("> draft text", ">"), Some(false));
}

#[test]
fn input_box_none_when_marker_absent() {
    // agent mid-output / no prompt rendered → cannot determine → None
    // (caller fails toward protection).
    assert_eq!(input_box_is_empty("just output, no prompt", "❯"), None);
    assert_eq!(input_box_is_empty("", "❯"), None);
}

#[test]
fn input_box_uses_bottom_most_marker_not_prose() {
    // #1944 prose-FP guard: a `❯`/`>` that appears MID-prose above the input
    // box must not be matched — only the bottom-most line whose first
    // non-blank char IS the marker counts as the live input prompt.
    let screen = "the agent printed ❯ in its output\nmore prose with > inside\n❯ ";
    assert_eq!(input_box_is_empty(screen, "❯"), Some(true));
    // a markdown blockquote above, real empty input below
    let screen2 = "> quoted line from agent output\nplain text\n> ";
    assert_eq!(input_box_is_empty(screen2, ">"), Some(true));
    // typed input below a blockquote → non-empty (still the bottom-most)
    let screen3 = "> quoted output\n> my actual draft";
    assert_eq!(input_box_is_empty(screen3, ">"), Some(false));
}

// ── #1948 v2: input_box_empty_probe (marker → placeholder → fallback) ──

#[test]
fn probe_marker_path_decides_directly() {
    // claude/codex/agy: marker present → decided by content after marker;
    // placeholder is ignored (None) for these backends.
    assert_eq!(
        input_box_empty_probe("out\n❯ ", Some("❯"), None),
        Some(true)
    );
    assert_eq!(
        input_box_empty_probe("out\n❯ typed", Some("❯"), None),
        Some(false)
    );
    // marker present but no prompt line in the tail (mid-output) → None
    // (fail-protect), NOT silently falling through to a non-existent placeholder.
    assert_eq!(input_box_empty_probe("just output", Some("❯"), None), None);
}

#[test]
fn probe_placeholder_path_for_kiro() {
    // kiro: no marker, placeholder VISIBLE → empty (deliver). Uses the real
    // captured placeholder text (live pane_snapshot of a cleared kiro pane).
    let ph = "ask a question or describe a task";
    let empty_screen = "  Kiro · auto · 13%\n\n ask a question or describe a task ↵\n  /copy";
    assert_eq!(
        input_box_empty_probe(empty_screen, None, Some(ph)),
        Some(true)
    );
    // typed → placeholder replaced/absent → None (fail toward protection).
    let typed_screen = "  Kiro · auto · 13%\n\n half-typed reply\n  /copy";
    assert_eq!(input_box_empty_probe(typed_screen, None, Some(ph)), None);
}

#[test]
fn probe_none_when_neither_marker_nor_placeholder() {
    // Shell/Raw / markerless backend → fall back to timestamp behavior.
    assert_eq!(input_box_empty_probe("$ ", None, None), None);
}

// ── #1948(b): input_box_dim_aware_empty (codex DIM-ghost) ──

#[test]
fn dim_aware_empty_when_ghost_is_dim() {
    // codex empty box: `› <dim ghost>` — the prompt is bold (idx 0, not dim),
    // every non-ws char after it is the DIM ghost → box empty (deliver).
    let text = "› Use /skills to list available skills";
    let n = text.chars().count();
    let dim: Vec<bool> = (0..n).map(|i| i != 0).collect();
    assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(true));
}

#[test]
fn dim_aware_nonempty_when_input_is_normal_intensity() {
    // a real draft: `› my draft` rendered at NORMAL intensity (no dim) → a
    // non-dim glyph after the marker → real input (protect).
    let text = "› my half-typed reply";
    let dim = vec![false; text.chars().count()];
    assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(false));
}

#[test]
fn dim_aware_empty_when_only_whitespace_after_marker() {
    let text = "some output\n› ";
    let dim = vec![false; text.chars().count()];
    assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(true));
}

#[test]
fn dim_aware_none_when_no_marker_line() {
    let text = "just output, no prompt";
    let dim = vec![false; text.chars().count()];
    assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), None);
}

#[test]
fn dim_aware_uses_bottom_most_marker_line() {
    // a `›` in DIM prose above + the real input box below at NORMAL intensity
    // → the bottom-most marker line (real input) decides → Some(false).
    let text = "› a dim quote in output\n› my real draft";
    let n = text.chars().count();
    let nl_char = text.split('\n').next().unwrap_or("").chars().count();
    // first line (+ its `\n`) dim; second line (real input) normal.
    let dim: Vec<bool> = (0..n).map(|i| i <= nl_char).collect();
    assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(false));
}

/// #1457: submitted (or never-typed) buffer → None → notifications deliver.
/// This is the submit-then-flush release path.
#[test]
fn draft_state_none_when_submitted_or_clean() {
    let home = tmp_home("draft_none");
    let now = chrono::Utc::now().timestamp_millis();
    // never typed
    assert_eq!(draft_state(&home, "fresh"), DraftState::None);
    // typed then submitted (submit newer) → clean
    write_ts(&home, "submitted", now - 1000, now - 100);
    assert_eq!(draft_state(&home, "submitted"), DraftState::None);
    std::fs::remove_dir_all(home).ok();
}

/// #1457: unsent draft, typed recently → Drafting (defer). Crucially this
/// holds regardless of how long the pause is (no 3s window) — the old
/// `is_composing` would have false-negatived after 3s of thinking.
#[test]
fn draft_state_drafting_when_typed_after_submit() {
    let home = tmp_home("draft_drafting");
    let now = chrono::Utc::now().timestamp_millis();
    // typed AFTER last submit, well within the escape window — but also
    // older than the old 3s window, proving we no longer false-negative.
    write_ts(&home, "a", now - 60_000, now - 120_000);
    assert_eq!(draft_state(&home, "a"), DraftState::Drafting);
    std::fs::remove_dir_all(home).ok();
}

/// #1457: unsent draft idle past the escape window → Abandoned (release).
/// Covers the "typed then deleted the draft / walked away" edge — the
/// escape valve is what bounds the otherwise-indefinite defer.
#[test]
fn draft_state_abandoned_past_escape_window() {
    let home = tmp_home("draft_abandoned");
    let now = chrono::Utc::now().timestamp_millis();
    // typed 400s ago, with a PRIOR submit (submit_ms>0) → past the 300s
    // escape → genuine "typed then walked away" → Abandoned (trickle kept).
    write_ts(&home, "a", now - 400_000, now - 500_000);
    assert_eq!(draft_state(&home, "a"), DraftState::Abandoned);
    std::fs::remove_dir_all(home).ok();
}

/// #1473: stale typed + NEVER submitted (submit_ms==0) → None, NOT
/// Abandoned. This is the regression case: an agent pane the operator
/// poked once but never composed in (e.g. codex reviewer) was trapped in
/// Abandoned, deferring its wakes forever. Must deliver normally now.
#[test]
fn draft_state_never_submitted_stale_is_none() {
    let home = tmp_home("draft_never_submit");
    let now = chrono::Utc::now().timestamp_millis();
    // typed 400s ago (past escape), submit_ms == 0 (never submitted).
    write_ts(&home, "a", now - 400_000, 0);
    assert_eq!(
        draft_state(&home, "a"),
        DraftState::None,
        "never-submitted stale pane must be None, not Abandoned (#1473)"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #1473 guard: the scoped fix must NOT weaken the active-draft protection.
/// A RECENT first-ever draft (typed now, submit_ms==0) is still Drafting —
/// proving a naive top-level `submit==0→None` (which would regress #1457)
/// was avoided.
#[test]
fn draft_state_first_draft_recent_still_drafting() {
    let home = tmp_home("draft_first");
    let now = chrono::Utc::now().timestamp_millis();
    write_ts(&home, "a", now - 1_000, 0); // typed 1s ago, never submitted
    assert_eq!(
        draft_state(&home, "a"),
        DraftState::Drafting,
        "recent first draft must stay protected (Drafting), not None (#1457 preserved)"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3663: write raw input/submit/cleared timestamps. Uses the literal
/// metadata key (not the production const) so this RED test compiles
/// against the base implementation.
fn write_ts_cleared(home: &Path, agent: &str, typed_ms: i64, submit_ms: i64, cleared_ms: i64) {
    write_ts(home, agent, typed_ms, submit_ms);
    if cleared_ms != 0 {
        agent_ops::save_metadata(home, agent, "last_cleared_epoch_ms", json!(cleared_ms));
    }
}

/// #3663: type-then-clear — cleared NEWER than the last keystroke reads None.
#[test]
fn draft_state_cleared_newer_than_typed_is_none_3663() {
    let home = tmp_home("draft_cleared");
    let now = chrono::Utc::now().timestamp_millis();
    write_ts_cleared(&home, "a", now - 30_000, now - 60_000, now - 5_000);
    assert_eq!(
        draft_state(&home, "a"),
        DraftState::None,
        "cleared-newer-than-typed must read None (#3663)"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3663: cleared TIED with the last keystroke keeps protection — the
/// observation cannot prove the box was empty AFTER the final keystroke
/// (same-ms clock granularity), so fail toward the live draft.
#[test]
fn draft_state_cleared_tied_with_typed_stays_drafting_3663() {
    let home = tmp_home("draft_cleared_tie");
    let now = chrono::Utc::now().timestamp_millis();
    write_ts_cleared(&home, "a", now - 30_000, now - 60_000, now - 30_000);
    assert_eq!(
        draft_state(&home, "a"),
        DraftState::Drafting,
        "cleared==typed tie must stay Drafting (fail toward protection)"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3663: cleared OLDER than the last keystroke stays Drafting — the
/// operator typed after the empty observation (a live draft).
#[test]
fn draft_state_cleared_older_than_typed_stays_drafting_3663() {
    let home = tmp_home("draft_cleared_stale");
    let now = chrono::Utc::now().timestamp_millis();
    write_ts_cleared(&home, "a", now - 5_000, now - 60_000, now - 30_000);
    assert_eq!(
        draft_state(&home, "a"),
        DraftState::Drafting,
        "stale cleared observation must not lift a live draft"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3663: legacy metadata (no cleared key) keeps the timestamp behavior.
#[test]
fn draft_state_without_cleared_keeps_timestamp_behavior_3663() {
    let home = tmp_home("draft_no_cleared");
    let now = chrono::Utc::now().timestamp_millis();
    write_ts(&home, "a", now - 30_000, now - 60_000);
    assert_eq!(
        draft_state(&home, "a"),
        DraftState::Drafting,
        "absent cleared key (legacy) must default 0 and keep Drafting"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3663 review F1: a FUTURE cleared observation (corrupt/clock-skewed —
/// TUI and daemon share the host clock, so cleared can never legitimately
/// postdate now) is IGNORED: the live draft keeps Drafting instead of being
/// hidden (which would clobber a real input line or bypass restart grace).
#[test]
fn draft_state_future_cleared_stays_drafting_3663() {
    let home = tmp_home("draft_cleared_future");
    let now = chrono::Utc::now().timestamp_millis();
    write_ts_cleared(&home, "a", now - 30_000, now - 60_000, now + 60_000);
    assert_eq!(
        draft_state(&home, "a"),
        DraftState::Drafting,
        "future cleared must fail closed (ignore), not hide the live draft"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #1457: escape valve releases ONE oldest notification, leaving the rest
/// queued (no clobbering batch).
#[test]
fn drain_one_pops_oldest_leaves_rest() {
    let home = tmp_home("drain_one");
    enqueue(&home, "a", "first").expect("enqueue first");
    enqueue(&home, "a", "second").expect("enqueue second");
    enqueue(&home, "a", "third").expect("enqueue third");
    let popped = drain_one(&home, "a").expect("one popped");
    assert_eq!(popped.text, "first", "oldest must be released first");
    assert_eq!(pending_count(&home, "a"), 2, "rest stay queued");
    assert_eq!(drain_one(&home, "a").expect("second pop").text, "second");
    assert_eq!(drain_one(&home, "a").expect("third pop").text, "third");
    assert!(drain_one(&home, "a").is_none(), "empty after draining all");
    std::fs::remove_dir_all(home).ok();
}

/// Concurrent-claim contract: a FRESH foreign draining file belongs to a
/// LIVE concurrent drain (e.g. the TUI flush mid-drain while the daemon's
/// per-tick flush scans the same agent). Re-reading it double-delivers
/// every line it contains. `drain` must claim work ONLY by atomically
/// renaming the live queue file; a fresh foreign draining file is left
/// untouched (only STALE ones — a crashed drainer's leftovers — are
/// recovered).
#[test]
fn drain_does_not_steal_fresh_foreign_draining_file() {
    let home = tmp_home("foreign_draining");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    enqueue(&home, "a", "claimed-by-peer").expect("enqueue");
    // Simulate a concurrent drainer that has JUST claimed the queue
    // (renamed it to its draining file and is about to inject).
    std::fs::rename(queue_path(&home, "a"), draining_path(&home, "a"))
        .expect("simulate peer claim");
    let got = drain(&home, "a");
    assert!(
        got.is_empty(),
        "a fresh foreign draining file must NOT be re-read — that \
         double-delivers the peer's claimed items: {got:?}"
    );
    assert_eq!(
        pending_count(&home, "a"),
        1,
        "the peer's claimed item still counts as pending (it owns delivery)"
    );
    std::fs::remove_dir_all(home).ok();
}

/// Crash recovery: a STALE draining file (its drainer died mid-flight —
/// including the legacy fixed-name file from a pre-claim-atomic binary)
/// must be folded into the next drain rather than stranding forever.
/// `stale_ms = 0` makes "stale" deterministic without mtime manipulation.
#[test]
fn drain_recovers_stale_draining_leftover() {
    let home = tmp_home("stale_draining");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    enqueue(&home, "a", "crashed-claim").expect("enqueue");
    std::fs::rename(queue_path(&home, "a"), draining_path(&home, "a"))
        .expect("simulate crashed drainer's leftover claim");
    let got = drain_settled_with_stale(&home, "a", 1, 0);
    assert_eq!(got.len(), 1, "stale leftover must be recovered");
    assert_eq!(got[0].text, "crashed-claim");
    assert_eq!(pending_count(&home, "a"), 0, "leftover consumed");
    std::fs::remove_dir_all(home).ok();
}

/// Reviewer challenge 3 (PR #1): a metadata anomaly (vanished file /
/// future mtime after a clock step) must read as STALE — skipping would
/// strand the leftover forever and permanently inflate pending_count.
/// Safe because the check only runs under the per-agent drain lock.
#[test]
fn stale_check_treats_metadata_anomaly_as_stale() {
    let missing = std::env::temp_dir()
        .join("agend-notification-queue-anomaly")
        .join("never-created.draining");
    assert!(
        draining_file_is_stale(&missing, 30_000),
        "unreadable metadata must classify as stale (recoverable), not strand"
    );
}

/// §3.9 concurrent-state harness: exactly-once delivery under racing
/// drainers. N threads drain the same agent concurrently; every enqueued
/// line must be delivered EXACTLY once across all threads. Serialized by
/// the per-agent OS drain lock — plain rename arbitration double-delivered
/// on Windows (PR #1 CI run 27248027241; both racing renames of one source
/// can succeed because handles survive renames).
#[test]
fn concurrent_drains_deliver_exactly_once() {
    let home = tmp_home("concurrent_drain");
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).ok();
    const ITEMS: usize = 50;
    for i in 0..ITEMS {
        enqueue(&home, "a", &format!("msg-{i}")).expect("enqueue");
    }
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let mut joins = Vec::new();
    for _ in 0..4 {
        let home = home.clone();
        let barrier = barrier.clone();
        joins.push(std::thread::spawn(move || {
            barrier.wait();
            let mut got = Vec::new();
            for _ in 0..8 {
                got.extend(drain(&home, "a"));
            }
            got
        }));
    }
    let mut all: Vec<String> = joins
        .into_iter()
        .flat_map(|j| j.join().expect("thread join"))
        .map(|n| n.text)
        .collect();
    all.sort();
    let unique: std::collections::HashSet<&String> = all.iter().collect();
    assert_eq!(
        unique.len(),
        all.len(),
        "no line may be delivered twice across concurrent drains"
    );
    assert_eq!(
        all.len(),
        ITEMS,
        "every enqueued line must be delivered exactly once"
    );
    std::fs::remove_dir_all(home).ok();
}

// ── #2028: false-empty under contention — single-shot caller honesty ──

/// Lock held by a peer → `Unavailable`, NEVER `Drained(empty)`. The
/// pre-#2028 collapse to an empty vec is what made `drain_one` report
/// "queue empty" under llvm-cov-grade load.
#[test]
fn try_drain_reports_unavailable_while_lock_held_2028() {
    let home = tmp_home("unavail-lock");
    enqueue(&home, "a", "queued").expect("enqueue");
    // #2666 uncovered facet: retrying acquire, not a raw one that flakes on a
    // transient Err under llvm-cov.
    let guard = acquire_drain_lock(&drain_lock_path(&home, "a"))
        .expect("lock open")
        .expect("lock acquired");
    assert!(
        matches!(
            try_drain_with_stale_threshold(&home, "a", STALE_DRAINING_MS),
            DrainAttempt::Unavailable
        ),
        "held lock must read as Unavailable, not empty"
    );
    drop(guard);
    match try_drain_with_stale_threshold(&home, "a", STALE_DRAINING_MS) {
        DrainAttempt::Drained(v) => {
            assert_eq!(v.len(), 1, "after release the claim drains the queue")
        }
        DrainAttempt::Unavailable => panic!("lock released — must drain"),
    }
    std::fs::remove_dir_all(home).ok();
}

/// Flusher contract UNCHANGED: `drain` (the next-tick-tolerant API)
/// still collapses contention to an empty vec — the holder delivers.
#[test]
fn flusher_drain_still_collapses_contention_to_empty_2028() {
    let home = tmp_home("flusher-collapse");
    enqueue(&home, "a", "queued").expect("enqueue");
    // #2666 uncovered facet: retrying acquire, not a raw one that flakes on a
    // transient Err under llvm-cov.
    let _guard = acquire_drain_lock(&drain_lock_path(&home, "a"))
        .expect("lock open")
        .expect("lock acquired");
    assert!(
        drain(&home, "a").is_empty(),
        "flusher API keeps the walk-away-empty semantics"
    );
    std::fs::remove_dir_all(home).ok();
}

/// drain_one outlasts a contention window (the healthy-peer shape: a live
/// flusher holds the lock for the duration of a rename+read). Deterministic:
/// the contention is injected via the path-keyed `force_contention` seam and
/// released only AFTER `drain_one` has provably hit the held lock at least
/// once (a structural wait on the acquire-attempt counter, not a wall-clock
/// sleep), so it genuinely RETRIES through the contention before succeeding —
/// with no timing bet for llvm-cov to stretch.
#[test]
fn drain_one_retries_through_short_contention_2028() {
    let home = tmp_home("drain-one-retry");
    enqueue(&home, "a", "the-item").expect("enqueue");
    let lp = drain_lock_path(&home, "a");
    test_hooks::reset_acquire_attempts(&lp);
    test_hooks::arm_contention(&lp);
    let h = home.clone();
    let worker = std::thread::spawn(move || drain_one(&h, "a"));
    // Wait until drain_one has attempted (and hit the held lock) at least
    // once, then release — proves the retry path without a fixed sleep.
    let lp_wait = lp.clone();
    while test_hooks::acquire_attempts(&lp_wait) < 1 {
        std::thread::yield_now();
    }
    test_hooks::clear_contention(&lp);
    let popped = worker.join().expect("join");
    assert_eq!(
        popped
            .expect("must retry past the contention, not report empty")
            .text,
        "the-item"
    );
    std::fs::remove_dir_all(home).ok();
}

/// True-empty stays a fast None — the retry loop only engages on
/// Unavailable, an actually-empty queue answers immediately.
#[test]
fn drain_one_true_empty_is_immediate_none_2028() {
    let home = tmp_home("drain-one-empty");
    let lp = drain_lock_path(&home, "a");
    test_hooks::reset_acquire_attempts(&lp);
    assert!(drain_one(&home, "a").is_none());
    // Structural (not wall-clock) proof the retry budget wasn't burned: a
    // truly-empty queue acquires the lock exactly ONCE and returns — the
    // retry loop only engages on Unavailable, never on a real empty drain.
    assert_eq!(
        test_hooks::acquire_attempts(&lp),
        1,
        "true-empty must drain in exactly one attempt, not burn the retry budget"
    );
    std::fs::remove_dir_all(home).ok();
}

/// reviewer5 (PR #2666): past the retry bound, `acquire_drain_lock` must
/// propagate the ORIGINAL (first) `Err`, not the last retry's — the contract
/// the doc + dispatch spec require. Forced-error mode makes every acquire
/// return a DISTINCT `Err`, so exhausting the bound proves the first one
/// survives.
#[test]
fn acquire_drain_lock_propagates_original_err_after_retry_exhaustion() {
    let home = tmp_home("acquire-err-identity");
    let lp = drain_lock_path(&home, "a");
    test_hooks::arm_forced_errors(&lp);
    let result = acquire_drain_lock(&lp);
    test_hooks::clear_forced_errors(&lp);
    // (`FileFlockGuard` isn't `Debug`, so match rather than `expect_err`.)
    let err = match result {
        Ok(_) => panic!("all acquires forced to Err → the wrapper must return an Err"),
        Err(e) => e,
    };
    assert_eq!(
        err.to_string(),
        "forced-acquire-err-1",
        "the ORIGINAL (first) error must survive retry exhaustion, not the last retry's"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #2666 uncovered-facet guard (flake recurrence #3, t-…44436-10): every
/// drain-lock acquisition in THIS file's test *setups* must route through the
/// retrying `acquire_drain_lock` (retries past a transient `Err`), NOT a raw
/// `store::try_acquire_file_lock` against a `&drain_lock_path(..)`, whose
/// spurious `Err` under the Coverage job's llvm-cov fd pressure panicked and
/// reddened CI three times. #2666 hardened the production acquire but not the
/// test-setup raw acquires; this pins the fix so a regressed raw acquire fails
/// loudly here instead of flaking under Coverage.
#[test]
fn test_setups_acquire_drain_lock_with_retry_not_raw() {
    // Post-split the test setups live here (tests.rs) while production lives
    // in ../notification_queue.rs — scan BOTH, concatenated, so a regressed
    // raw acquire in either file fails loudly instead of flaking under Coverage.
    let src = concat!(
        include_str!("../notification_queue.rs"),
        include_str!("tests.rs"),
    );
    // Assemble the forbidden call form at runtime so THIS test's own source
    // never contains the needle verbatim (no self-match). The production
    // acquire + `test_hooks::raw_acquire` call `try_acquire_file_lock(lock_path)`
    // / `(p)`, so this drain-lock-path-specific form matches ONLY test setups.
    let raw_setup_acquire = format!("{}{}", "try_acquire_file_lock", "(&drain_lock_path");
    let hits = src.matches(raw_setup_acquire.as_str()).count();
    assert_eq!(
        hits, 0,
        "drain-lock test setups must use `acquire_drain_lock` (retry-past-Err), \
         not a raw drain-lock `try_acquire_file_lock` — see #2666 / flake recurrence #3"
    );
}

// ── #2965: input-activity coalesce ──

/// RED: `record_input_activity` must NOT perform a durable metadata
/// write on every call — the write is deferred to
/// `flush_pending_input_activity`.
#[test]
fn input_activity_coalesced_not_flushed_immediately() {
    let home = tmp_home("coalesce_not_imm");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    record_input_activity(&home, "agent1");
    let (typed, _) = read_input_submit_timestamps(&home, "agent1");
    assert_eq!(
        typed, 0,
        "record_input_activity must not write to disk immediately — \
         keystrokes coalesce until the next flush (got typed={typed})"
    );
    flush_pending_input_activity(&home);
    assert_eq!(pending_input_count_for(&home), 0);
    std::fs::remove_dir_all(home).ok();
}

/// RED: after an explicit flush the coalesced timestamp must be
/// durable on disk and readable by the draft-state machinery.
#[test]
fn coalesced_input_flushed_on_explicit_call() {
    let home = tmp_home("coalesce_flush");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    record_input_activity(&home, "agent1");
    flush_pending_input_activity(&home);
    let (typed, _) = read_input_submit_timestamps(&home, "agent1");
    assert!(
        typed > 0,
        "flush_pending_input_activity must persist the pending timestamp \
         (got typed=0 — flush is a no-op or lost the entry)"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #2965: N rapid keystrokes must coalesce into a single pending
/// entry (same agent), not N entries — proving the upsert, not
/// just the deferral.
#[test]
fn multiple_keystrokes_coalesce_to_one_pending_entry() {
    let home = tmp_home("multi_coalesce");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    for _ in 0..10 {
        record_input_activity(&home, "agent1");
    }
    assert_eq!(
        pending_input_count_for(&home),
        1,
        "10 keystrokes for the same agent must coalesce to 1 pending entry"
    );
    flush_pending_input_activity(&home);
    let (typed, _) = read_input_submit_timestamps(&home, "agent1");
    assert!(
        typed > 0,
        "coalesced timestamp must be persisted after flush"
    );
    assert_eq!(
        pending_input_count_for(&home),
        0,
        "flush must drain all pending entries for this home"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3321: submit joins input in the pending latest-wins pair and the
/// ordering (input < submit) produces no false live-draft after flush.
#[test]
fn submit_immediate_no_false_draft_after_flush() {
    let home = tmp_home("submit_order");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    record_input_activity(&home, "agent1");
    record_submit_activity(&home, "agent1");
    // Both activity fields stay in memory until the batch flush.
    let (typed_pre, submit_pre) = read_input_submit_timestamps(&home, "agent1");
    assert_eq!(typed_pre, 0, "input must still be pending (not flushed)");
    assert_eq!(submit_pre, 0, "submit must stay pending with input");
    // Now flush the pair.
    flush_pending_input_activity(&home);
    let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
    assert!(
        submit >= typed,
        "submit (called after input) must be >= typed — no false draft \
         (got typed={typed} submit={submit})"
    );
    assert_ne!(
        draft_state(&home, "agent1"),
        DraftState::Drafting,
        "after submit, draft state must not be Drafting"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3321 RED: both activity fields must stay in memory until one flush
/// performs one locked batch RMW, preserving unrelated metadata.
#[test]
fn activity_recorders_are_memory_only_and_flush_as_one_batch_3321() {
    let home = tmp_home("activity-batch-3321");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    agent_ops::save_metadata(&home, "agent1", "role", json!("dev"));
    agent_ops::reset_metadata_rmw_count();

    record_input_activity(&home, "agent1");
    record_submit_activity(&home, "agent1");
    assert_eq!(
        read_input_submit_timestamps(&home, "agent1"),
        (0, 0),
        "#3321: recorders must not touch metadata on the input path"
    );
    assert_eq!(pending_input_count_for(&home), 1);

    flush_pending_input_activity(&home);
    let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
    assert!(typed > 0, "#3321: input timestamp must flush");
    assert!(submit > 0, "#3321: submit timestamp must flush");
    let metadata = std::fs::read_to_string(home.join("metadata/agent1.json")).unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(
        metadata["role"], "dev",
        "#3321: unrelated metadata survives"
    );
    assert_eq!(
        agent_ops::take_metadata_rmw_count(),
        1,
        "#3321: input+submit must use one batch RMW"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn activity_requeue_merges_each_field_by_max_3321() {
    let mut target = PendingActivity {
        home: PathBuf::from("/home/agent"),
        agent: "agent1".to_owned(),
        input_ms: Some(200),
        submit_ms: Some(400),
        cleared_ms: None,
    };
    let newer_input = PendingActivity {
        home: target.home.clone(),
        agent: target.agent.clone(),
        input_ms: Some(300),
        submit_ms: Some(100),
        cleared_ms: None,
    };
    merge_activity(&mut target, &newer_input);
    assert_eq!(target.input_ms, Some(300));
    assert_eq!(target.submit_ms, Some(400));
}

/// #3663: the cleared leg merges by max like input/submit.
#[test]
fn activity_requeue_merges_cleared_by_max_3663() {
    let mut target = PendingActivity {
        home: PathBuf::from("/home/agent"),
        agent: "agent1".to_owned(),
        input_ms: None,
        submit_ms: None,
        cleared_ms: Some(100),
    };
    let newer = PendingActivity {
        home: target.home.clone(),
        agent: target.agent.clone(),
        input_ms: None,
        submit_ms: None,
        cleared_ms: Some(300),
    };
    merge_activity(&mut target, &newer);
    assert_eq!(target.cleared_ms, Some(300));
}

/// #3321 RED: a held exact metadata lock must not park the flush worker.
/// The channel barrier proves the lock is held before the flush begins;
/// the timeout only bounds a regression to the old blocking implementation.
#[test]
fn contended_activity_flush_returns_and_requeues_without_loss_3321() {
    let home = tmp_home("activity-contention-3321");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    agent_ops::save_metadata(&home, "agent1", "role", json!("dev"));
    record_input_activity(&home, "agent1");
    record_submit_activity(&home, "agent1");
    let (release_tx, holder) = hold_metadata_lock(&home);

    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let flush_home = home.clone();
    let flush_thread = std::thread::spawn(move || {
        flush_pending_input_activity(&flush_home);
        done_tx.send(()).unwrap();
    });
    let returned_while_held = done_rx.recv_timeout(Duration::from_millis(250)).is_ok();
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    flush_thread.join().unwrap();
    assert!(
        returned_while_held,
        "#3321: periodic flush must use try-lock, never block on metadata flock"
    );
    assert_eq!(
        pending_input_count_for(&home),
        1,
        "#3321: contention must retain the unified pending entry"
    );

    flush_pending_input_activity(&home);
    let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
    assert!(
        typed > 0 && submit > 0,
        "#3321: retry must persist both fields"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3321 RED: recording submit activity itself must remain nonblocking
/// while another process owns the exact metadata lock.
#[test]
fn activity_recorders_return_while_metadata_lock_is_held_3321() {
    let home = tmp_home("activity-record-contention-3321");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    agent_ops::save_metadata(&home, "agent1", "role", json!("dev"));
    let (release_tx, holder) = hold_metadata_lock(&home);

    let record_home = home.clone();
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let recorder = std::thread::spawn(move || {
        record_input_activity(&record_home, "agent1");
        record_submit_activity(&record_home, "agent1");
        done_tx.send(()).unwrap();
    });
    let returned_while_held = done_rx.recv_timeout(Duration::from_millis(250)).is_ok();
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    recorder.join().unwrap();
    assert!(
        returned_while_held,
        "#3321: activity recorders must not wait on metadata flock"
    );
    assert_eq!(pending_input_count_for(&home), 1);
    flush_pending_input_activity(&home);
    let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
    assert!(typed > 0 && submit > 0);
    std::fs::remove_dir_all(home).ok();
}

/// #3321 RED: an atomic-write error after draining must requeue the latest
/// fields so a later retry cannot silently lose the activity pair.
#[test]
fn activity_flush_requeues_after_atomic_write_error_3321() {
    let home = tmp_home("activity-error-3321");
    std::fs::create_dir_all(home.join("metadata")).unwrap();
    record_input_activity(&home, "agent1");
    record_submit_activity(&home, "agent1");
    let metadata_path = agent_ops::metadata_path_resolved(&home, "agent1");
    crate::store::fail_next_atomic_write_for_test(&metadata_path);

    flush_pending_input_activity(&home);
    assert_eq!(
        pending_input_count_for(&home),
        1,
        "#3321: failed persistence must requeue, not drop, the pair"
    );
    flush_pending_input_activity(&home);
    let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
    assert!(
        typed > 0 && submit > 0,
        "#3321: retry after error must persist"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn activity_teardown_flush_persists_remaining_pair_3321() {
    let home = tmp_home("activity-teardown-3321");
    record_input_activity(&home, "agent1");
    record_submit_activity(&home, "agent1");
    flush_pending_activity_at_teardown(&home);
    let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
    assert!(typed > 0 && submit > 0);
    assert_eq!(pending_input_count_for(&home), 0);
    std::fs::remove_dir_all(home).ok();
}
