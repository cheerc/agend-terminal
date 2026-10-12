//! #28 RED: `create_instance` must accept `effort` on the single path and
//! validate it BEFORE any fleet entry or instance exists.
//!
//! These drive the REAL caller entry point (`spawn_single_instance_impl` with
//! a mock `spawn_fn`, the #964 harness) — not a mid-pipeline helper — so the
//! pre-mutation ordering contract is what is actually pinned. On the frozen
//! base (no `effort` on the create wire) the new-feature tests below must FAIL;
//! after the implementation each must pass. This file is RED-only scaffolding
//! for the immutable RED→GREEN; the implementation must not edit these assertions.

use super::instance_state::spawn::spawn_single_instance_impl;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

fn red_home(slug: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("agend-28red-{slug}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&dir), "instances: {}\n").unwrap();
    dir
}

fn ok_spawn_fn(_h: &Path, _req: &Value) -> anyhow::Result<Value> {
    Ok(json!({"ok": true, "result": {}}))
}

fn fleet_has(home: &Path, name: &str) -> bool {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .map(|c| c.instance_names().iter().any(|n| n == name))
        .unwrap_or(false)
}

/// The create_instance tool schema must advertise `effort`.
#[test]
fn create_instance_schema_advertises_effort_28red() {
    let def = super::super::tools::def_create_instance();
    let props = &def["inputSchema"]["properties"];
    assert!(
        props.get("effort").is_some(),
        "#28 RED: create_instance schema must carry an `effort` property"
    );
}

/// Supported effort succeeds and persists the entry — the intent is durable
/// with no restart needed. The mock spawn_fn asserts the argv reaches SPAWN
/// with `--effort <val>` injected.
#[test]
fn single_create_effort_reaches_first_spawn_and_entry_28red() {
    let home = red_home("first-spawn");
    let seen_args = Arc::new(parking_lot::Mutex::new(None));
    let seen_args_clone = seen_args.clone();
    let spawn_fn = move |_h: &Path, req: &Value| -> anyhow::Result<Value> {
        *seen_args_clone.lock() = req["params"]["args"].as_str().map(String::from);
        Ok(json!({"ok": true, "result": {}}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-single", "backend": "claude", "effort": "high"}),
        &spawn_fn,
        None,
    );
    assert!(
        result.get("error").is_none(),
        "#28 RED: supported effort must succeed: {result}"
    );
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let cfg = fleet.instances.get("eff-single").expect("entry must exist");
    assert_eq!(
        cfg.effort.as_deref(),
        Some("high"),
        "#28 RED: entry must persist effort: {result}"
    );
    let captured = seen_args.lock().clone();
    assert!(
        captured.as_deref().unwrap_or("").contains("--effort high"),
        "#28 RED: spawn_fn must receive --effort high in args; got {captured:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Codex backend uses `-c model_reasoning_effort="..."` config syntax.
#[test]
fn single_create_codex_effort_reaches_first_spawn_and_entry_28red() {
    let home = red_home("codex-spawn");
    let seen_args = Arc::new(parking_lot::Mutex::new(None));
    let seen_args_clone = seen_args.clone();
    let spawn_fn = move |_h: &Path, req: &Value| -> anyhow::Result<Value> {
        *seen_args_clone.lock() = req["params"]["args"].as_str().map(String::from);
        Ok(json!({"ok": true, "result": {}}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-codex", "backend": "codex", "effort": "high"}),
        &spawn_fn,
        None,
    );
    assert!(
        result.get("error").is_none(),
        "#28 RED: codex supported effort must succeed: {result}"
    );
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let cfg = fleet.instances.get("eff-codex").expect("entry must exist");
    assert_eq!(
        cfg.effort.as_deref(),
        Some("high"),
        "#28 RED: codex entry must persist effort: {result}"
    );
    let captured = seen_args.lock().clone();
    assert!(
        captured
            .as_deref()
            .unwrap_or("")
            .contains("-c model_reasoning_effort=\"high\""),
        "#28 RED: codex spawn_fn must receive -c model_reasoning_effort=\"high\"; got {captured:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Empty string effort is treated as unset (F1 decided: no-op, no CLEAR, defaults kept).
#[test]
fn create_instance_effort_empty_string_ignored_28red() {
    let home = red_home("empty-string");
    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-empty", "backend": "claude", "effort": ""}),
        &ok_spawn_fn,
        None,
    );
    assert!(
        result.get("error").is_none(),
        "#28 RED: empty effort string must be treated as unset: {result}"
    );
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let cfg = fleet.instances.get("eff-empty").expect("entry must exist");
    assert_eq!(
        cfg.effort, None,
        "#28 RED: empty effort must persist None in fleet entry"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// A globally-unknown value is a structured pre-mutation refusal.
#[test]
fn create_instance_effort_unknown_value_refuses_before_entry_28red() {
    let home = red_home("bad-value");
    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-badval", "backend": "claude", "effort": "ultra"}),
        &ok_spawn_fn,
        None,
    );
    assert_eq!(
        result["code"].as_str(),
        Some("invalid_effort_value"),
        "#28 RED: unknown value must refuse with invalid_effort_value: {result}"
    );
    assert!(
        !fleet_has(&home, "eff-badval"),
        "#28 RED: refusal must leave zero fleet entry"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Codex + xhigh is globally legal but per-backend illegal (F2 decided:
/// same `invalid_effort_value` code) — and must refuse BEFORE any entry.
#[test]
fn create_instance_effort_codex_xhigh_refuses_before_entry_28red() {
    let home = red_home("codex-xhigh");
    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-cx", "backend": "codex", "effort": "xhigh"}),
        &ok_spawn_fn,
        None,
    );
    assert_eq!(
        result["code"].as_str(),
        Some("invalid_effort_value"),
        "#28 RED: codex+xhigh must refuse with invalid_effort_value: {result}"
    );
    assert!(
        !fleet_has(&home, "eff-cx"),
        "#28 RED: refusal must leave zero fleet entry"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// A backend with no declared effort capability refuses with its own code.
#[test]
fn create_instance_effort_unsupported_backend_refuses_before_entry_28red() {
    let home = red_home("no-cap");
    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-nocap", "backend": "shell", "effort": "high"}),
        &ok_spawn_fn,
        None,
    );
    assert_eq!(
        result["code"].as_str(),
        Some("effort_unsupported_backend"),
        "#28 RED: no-capability backend must refuse with effort_unsupported_backend: {result}"
    );
    assert!(
        !fleet_has(&home, "eff-nocap"),
        "#28 RED: refusal must leave zero fleet entry"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// A hand-written effort setting in `args` conflicts (F4 decided: hard
/// refuse reusing the scanner, `args_conflict_confirmed` shape).
#[test]
fn create_instance_effort_args_conflict_refuses_before_entry_28red() {
    let home = red_home("conflict");
    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-conf", "backend": "claude", "args": "--effort low", "effort": "high"}),
        &ok_spawn_fn,
        None,
    );
    assert_eq!(
        result["code"].as_str(),
        Some("args_conflict_confirmed"),
        "#28 RED: hand-written effort in args must refuse with args_conflict_confirmed: {result}"
    );
    assert!(
        !fleet_has(&home, "eff-conf"),
        "#28 RED: refusal must leave zero fleet entry"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Flags appearing after a bare `--` are payload text and must NOT trigger conflict.
#[test]
fn create_instance_effort_args_payload_after_dash_dash_no_conflict_28red() {
    let home = red_home("payload-exemption");
    let seen_args = Arc::new(parking_lot::Mutex::new(None));
    let seen_args_clone = seen_args.clone();
    let spawn_fn = move |_h: &Path, req: &Value| -> anyhow::Result<Value> {
        *seen_args_clone.lock() = req["params"]["args"].as_str().map(String::from);
        Ok(json!({"ok": true, "result": {}}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-payload", "backend": "claude", "args": "-- --effort low", "effort": "high"}),
        &spawn_fn,
        None,
    );
    assert!(
        result.get("error").is_none(),
        "#28 RED: payload after -- must not conflict: {result}"
    );
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let cfg = fleet
        .instances
        .get("eff-payload")
        .expect("entry must exist");
    assert_eq!(
        cfg.effort.as_deref(),
        Some("high"),
        "#28 RED: effort must persist when args flag is after --"
    );
    let captured = seen_args.lock().clone();
    assert!(
        captured
            .as_deref()
            .unwrap_or("")
            .starts_with("--effort high --"),
        "#28 RED: --effort high must be inserted before --; got {captured:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Batch mode (team path) carrying a non-empty effort refuses single-only —
/// never silently ignored (F3 decided: code `unsupported_mode`).
#[test]
fn create_instance_effort_batch_mode_refuses_single_only_28red() {
    let home = red_home("batch");
    let result = super::instance::handle_create_instance(
        &home,
        &json!({"team": "effteam", "backend": "claude", "count": 2, "effort": "high"}),
        "lead",
        None,
    );
    assert_eq!(
        result["code"].as_str(),
        Some("unsupported_mode"),
        "#28 RED: batch effort must refuse with unsupported_mode: {result}"
    );
    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    assert!(
        cfg.instance_names()
            .iter()
            .all(|n| !n.starts_with("effteam-")),
        "#28 RED: batch refusal must leave zero member entries"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Legal single-join (name + team) passes effort through to the single instance.
#[test]
fn create_instance_effort_named_single_join_persists_effort_28red() {
    let home = red_home("single-join");
    *super::instance_state::spawn::SPAWN_OVERRIDE.lock() = Some((home.clone(), ok_spawn_fn));

    let result = super::instance::handle_create_instance(
        &home,
        &json!({"name": "eff-join", "team": "jointeam", "backend": "claude", "effort": "high"}),
        "lead",
        None,
    );
    *super::instance_state::spawn::SPAWN_OVERRIDE.lock() = None;

    assert!(
        result.get("error").is_none(),
        "#28 RED: single-join with effort must succeed: {result}"
    );
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let cfg = fleet.instances.get("eff-join").expect("entry must exist");
    assert_eq!(
        cfg.effort.as_deref(),
        Some("high"),
        "#28 RED: single-join must persist effort"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Regression: model-only and empty-effort calls behave exactly as before.
#[test]
fn create_instance_without_effort_unchanged_28red() {
    let home = red_home("legacy");
    let result = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "eff-legacy", "backend": "claude", "model": "opus"}),
        &ok_spawn_fn,
        None,
    );
    assert!(
        result.get("error").is_none(),
        "#28 RED: model-only create must keep working: {result}"
    );
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let cfg = fleet.instances.get("eff-legacy").expect("entry must exist");
    assert_eq!(cfg.effort, None, "#28 RED: no effort must persist none");
    let _ = std::fs::remove_dir_all(&home);
}
