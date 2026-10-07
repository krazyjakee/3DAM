//! Optional writer tracing for the manual production scan harness. No callbacks are installed
//! during normal operation. Counters are process-wide; the harness uses one measured catalog.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static ASSET_UPDATES: AtomicU64 = AtomicU64::new(0);
static STATEMENTS: AtomicU64 = AtomicU64::new(0);
static TRIGGER_STATEMENTS: AtomicU64 = AtomicU64::new(0);
static COMMITS: AtomicU64 = AtomicU64::new(0);
static WRITER_NANOS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct ScanSqlMetrics {
    pub writer_statements: u64,
    pub asset_rows_updated: u64,
    pub trigger_statements: u64,
    pub writer_commits: u64,
    pub writer_elapsed_ns: u64,
}

fn trace(event: rusqlite::trace::TraceEvent<'_>) {
    match event {
        rusqlite::trace::TraceEvent::Stmt(_, sql) => {
            if sql.starts_with("--") {
                TRIGGER_STATEMENTS.fetch_add(1, Ordering::Relaxed);
            } else {
                STATEMENTS.fetch_add(1, Ordering::Relaxed);
                if sql.trim().eq_ignore_ascii_case("COMMIT") {
                    COMMITS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        rusqlite::trace::TraceEvent::Profile(_, elapsed) => {
            WRITER_NANOS.fetch_add(
                elapsed.as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
        _ => {}
    }
}

impl Store {
    /// Enable optional process-wide counters on this catalog's writer, including trigger work.
    /// Read-pool statements and physical I/O are deliberately outside this metric.
    #[doc(hidden)]
    pub fn enable_scan_sql_metrics(&self) {
        let writer = self.write();
        writer
            .update_hook(Some(|action, _: &str, table: &str, _: i64| {
                if action == rusqlite::hooks::Action::SQLITE_UPDATE && table == "asset" {
                    ASSET_UPDATES.fetch_add(1, Ordering::Relaxed);
                }
            }))
            .expect("install manual scan row metrics");
        writer.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT
                | rusqlite::trace::TraceEventCodes::SQLITE_TRACE_PROFILE,
            Some(trace),
        );
    }

    #[doc(hidden)]
    pub fn scan_sql_metrics(&self) -> ScanSqlMetrics {
        ScanSqlMetrics {
            writer_statements: STATEMENTS.load(Ordering::Relaxed),
            asset_rows_updated: ASSET_UPDATES.load(Ordering::Relaxed),
            trigger_statements: TRIGGER_STATEMENTS.load(Ordering::Relaxed),
            writer_commits: COMMITS.load(Ordering::Relaxed),
            writer_elapsed_ns: WRITER_NANOS.load(Ordering::Relaxed),
        }
    }
}
