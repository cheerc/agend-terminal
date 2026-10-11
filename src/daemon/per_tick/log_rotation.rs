//! #914 hourly cleanup tick — hard backstop on the rolling appender's
//! disk footprint. `tracing_appender::rolling::Builder::max_log_files`
//! gives us a count cap; this handler gives us a bytes cap so a single
//! pathological day (~800 MB seen during heavy dev sessions, see #914)
//! cannot blow past the budget.
//!
//! Also maintains the Unix `daemon.log` symlink so operators tailing
//! that path keep tracking the active rotated file across midnight
//! boundaries.

use super::{PerTickHandler, TickContext};

pub(crate) struct LogRotationHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl LogRotationHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for LogRotationHandler {
    fn name(&self) -> &'static str {
        "log_rotation"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let max_bytes = std::env::var("AGEND_LOG_MAX_BYTES")
            .ok()
            .and_then(|v| crate::logging::parse_size(&v))
            .unwrap_or(crate::logging::DEFAULT_MAX_BYTES);
        let removed = crate::logging::cleanup_oversize_logs(ctx.home, max_bytes);
        if removed > 0 {
            tracing::info!(
                removed,
                max_bytes,
                "log_rotation: pruned oversize daemon.log.* entries"
            );
        }
        // #3669: hourly safety net for JSONL rotated generations. The
        // on-write path already prunes (rotate + age cap under the append
        // lock), so this only fires when a writer died mid-rotation or an
        // older build left residue: it sweeps stale/over-cap `.N`
        // generations of the daemon-owned stores WITHOUT touching the live
        // files (never deletes `.jsonl` itself — that would drop audit
        // records; the live file only ever shrinks via on-write rotation).
        let swept = crate::jsonl_retention::sweep_rotated_generations(ctx.home);
        if swept > 0 {
            tracing::info!(swept, "log_rotation: swept stale JSONL generations");
        }
        crate::logging::update_daemon_log_symlink_unix(ctx.home);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn fires_on_first_tick_then_every_n() {
        let h = LogRotationHandler::new(4);
        let fires: Vec<bool> = (0..9).map(|_| h.gate.fire()).collect();
        assert_eq!(
            fires,
            vec![true, false, false, false, true, false, false, false, true]
        );
    }

    #[test]
    fn real_rotation_tick_updates_daemon_symlink_to_real_producer_log_78() {
        use std::io::Write;
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let home =
            std::env::temp_dir().join(format!("agend-tick-rot-78-{}-{id}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);

        let mut appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("daemon")
            .filename_suffix(crate::logging::LOG_FILENAME_SUFFIX)
            .build(&home)
            .expect("build appender");
        appender.write_all(b"tick test log\n").unwrap();
        appender.flush().unwrap();

        let registry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let handler = LogRotationHandler::new(1);
        handler.run(&ctx);

        #[cfg(unix)]
        {
            let link = home.join("daemon.log");
            assert!(link.exists(), "daemon.log symlink must be created on tick");
            let target = std::fs::read_link(&link).expect("symlink");
            let target_str = target.to_string_lossy();
            assert!(
                target_str.starts_with("daemon.") && target_str.ends_with(".log"),
                "symlink must point to real producer log file, got {target_str}"
            );
        }

        std::fs::remove_dir_all(&home).ok();
    }
}
