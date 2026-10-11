#![allow(clippy::unwrap_used)]

use super::*;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const API_MAX_CONNS: usize = 32;

fn test_home(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "agend-api-capacity-test-{}-{}-{}",
        std::process::id(),
        label,
        id
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn setup_test_server(
    label: &str,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    Arc<AtomicBool>,
    u16,
    crate::auth_cookie::Cookie,
) {
    let home = test_home(label);
    let run_dir = crate::daemon::run_dir(&home);
    std::fs::create_dir_all(&run_dir).unwrap();
    crate::daemon::write_daemon_id(&run_dir);
    crate::auth_cookie::issue(&run_dir).unwrap();
    let token = crate::auth_cookie::read_operator_token(&run_dir).unwrap();

    let registry = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let externals = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let api_home = home.clone();
    let s_registry = Arc::clone(&registry);
    let s_configs = Arc::clone(&configs);
    let s_externals = Arc::clone(&externals);
    let s_shutdown = Arc::clone(&shutdown);

    std::thread::Builder::new()
        .name(format!("test_api_{label}"))
        .spawn(move || {
            serve_with_ready(
                &api_home,
                s_registry,
                s_shutdown,
                s_configs,
                s_externals,
                None,
                RestartCapability::Daemon,
                None,
                ready_tx,
                None,
            );
        })
        .unwrap();

    ready_rx.recv().unwrap().unwrap();
    let port_str = std::fs::read_to_string(run_dir.join("api.port")).unwrap();
    let port = port_str.trim().parse::<u16>().unwrap();
    (home, run_dir, shutdown, port, token)
}

fn connect_and_handshake(
    port: u16,
    token: &crate::auth_cookie::Cookie,
) -> (TcpStream, BufReader<TcpStream>) {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_nodelay(true).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, token).unwrap();
    (writer, reader)
}

#[test]
fn capacity_rejection_emits_inband_ndjson_error_22() {
    let (home, _run_dir, _shutdown, port, token) = setup_test_server("capacity-rejection-inband");

    // Establish API_MAX_CONNS (32) connections to exhaust capacity.
    let mut holders = Vec::with_capacity(API_MAX_CONNS);
    for _ in 0..API_MAX_CONNS {
        holders.push(connect_and_handshake(port, &token));
    }

    // Connection 33 attempts to connect; it must be rejected with in-band NDJSON.
    let stream33 = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream33.set_nodelay(true).unwrap();
    stream33
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut reader33 = BufReader::new(stream33);
    let mut line = String::new();
    let bytes_read = reader33.read_line(&mut line).unwrap_or(0);

    // In-band rejection must deliver non-empty NDJSON with reason="capacity".
    assert!(
        bytes_read > 0 && !line.trim().is_empty(),
        "client must not receive zero response bytes on capacity rejection (got EOF / 0 bytes)"
    );

    let parsed: serde_json::Value =
        serde_json::from_str(line.trim()).expect("rejection payload must be valid JSON");
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "rejection payload must indicate ok: false"
    );
    assert_eq!(
        parsed.get("reason").and_then(|v| v.as_str()),
        Some("capacity"),
        "rejection payload must carry reason: capacity"
    );
    assert!(
        parsed
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("at capacity"),
        "error description must mention 'at capacity'"
    );

    // Clean up
    drop(holders);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn conns_released_after_client_closes_22() {
    let (home, _run_dir, _shutdown, port, token) = setup_test_server("capacity-release-slots");

    let mut holders = Vec::with_capacity(API_MAX_CONNS);
    for _ in 0..API_MAX_CONNS {
        holders.push(connect_and_handshake(port, &token));
    }

    // Connection 33 is rejected.
    let stream33 = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream33
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut reader33 = BufReader::new(stream33);
    let mut line = String::new();
    let _ = reader33.read_line(&mut line);

    // Release all 32 connections.
    drop(holders);

    // Allow daemon worker threads to detect EOF and release their ConnSlot.
    let mut reconnected = false;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(25));
        if let Ok((_w, _r)) = std::panic::catch_unwind(|| connect_and_handshake(port, &token)) {
            reconnected = true;
            break;
        }
    }
    assert!(
        reconnected,
        "new connection must succeed after previous connections released their slots"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn client_handshake_observes_capacity_rejection_22() {
    let (home, _run_dir, _shutdown, port, token) =
        setup_test_server("capacity-handshake-rejection");

    let mut holders = Vec::with_capacity(API_MAX_CONNS);
    for _ in 0..API_MAX_CONNS {
        holders.push(connect_and_handshake(port, &token));
    }

    // Client connects and performs client_handshake_ndjson.
    // It should receive the in-band rejection line instead of "server closed before reply".
    let stream33 = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream33
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut writer33 = stream33.try_clone().unwrap();
    let mut reader33 = BufReader::new(stream33);

    let err = crate::auth_cookie::client_handshake_ndjson(&mut reader33, &mut writer33, &token)
        .expect_err("handshake must fail on rejected connection");
    let err_str = err.to_string();
    assert!(
        !err_str.contains("server closed before reply"),
        "client handshake must observe server rejection, not bare EOF 'server closed before reply', got: {err_str}"
    );

    drop(holders);
    let _ = std::fs::remove_dir_all(&home);
}
