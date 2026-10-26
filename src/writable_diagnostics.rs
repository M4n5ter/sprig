//! Experimental, opt-in measurement only. No journal or namespace decisions use it.
use serde::Serialize;
use std::cell::RefCell;
use std::collections::VecDeque;

const CAPACITY: usize = 65_536;

#[derive(Serialize)]
struct Event {
    operation: &'static str,
    started_ns: u64,
    finished_ns: u64,
    sequence: u64,
    scanned_nodes: u64,
    success: bool,
    repeats_successful_sync: bool,
}

#[derive(Default, Serialize)]
struct Totals {
    calls: u64,
    nanoseconds: u64,
    maximum_nanoseconds: u64,
    scanned_nodes: u64,
    repeated_successful_syncs: u64,
    repeated_successful_sync_nanoseconds: u64,
}

#[derive(Serialize)]
pub(super) struct Diagnostics {
    schema: &'static str,
    clock: &'static str,
    capacity: usize,
    overwritten_events: u64,
    clock_failures: u64,
    directory_attr: Totals,
    journal_sync: Totals,
    events: VecDeque<Event>,
    #[serde(skip)]
    last_successful_sync: Option<u64>,
}

impl Diagnostics {
    pub(super) fn from_environment() -> Option<RefCell<Self>> {
        if std::env::var_os("SPRIG_WRITABLE_DIAGNOSTICS").as_deref()
            != Some(std::ffi::OsStr::new("1"))
            || monotonic_ns().is_none()
        {
            return None;
        }
        Some(RefCell::new(Self::new(CAPACITY)))
    }

    fn new(capacity: usize) -> Self {
        Self {
            schema: "sprig-writable-diagnostics-v1",
            clock: "CLOCK_MONOTONIC",
            capacity,
            overwritten_events: 0,
            clock_failures: 0,
            directory_attr: Totals::default(),
            journal_sync: Totals::default(),
            events: VecDeque::with_capacity(capacity),
            last_successful_sync: None,
        }
    }

    pub(super) fn attr(&mut self, started: Option<u64>, sequence: u64, scanned: usize) {
        self.record(
            "directory_attr",
            started,
            monotonic_ns(),
            sequence,
            scanned as u64,
            true,
        );
    }

    pub(super) fn sync(&mut self, started: Option<u64>, sequence: u64, success: bool) {
        self.record(
            "journal_sync",
            started,
            monotonic_ns(),
            sequence,
            0,
            success,
        );
    }

    fn record(
        &mut self,
        operation: &'static str,
        started: Option<u64>,
        finished: Option<u64>,
        sequence: u64,
        scanned: u64,
        success: bool,
    ) {
        let repeat = operation == "journal_sync" && self.last_successful_sync == Some(sequence);
        if operation == "journal_sync" {
            self.last_successful_sync = success.then_some(sequence);
        }
        let (Some(started_ns), Some(finished_ns)) = (started, finished) else {
            self.clock_failures = self.clock_failures.saturating_add(1);
            return;
        };
        let Some(duration) = finished_ns.checked_sub(started_ns) else {
            self.clock_failures = self.clock_failures.saturating_add(1);
            return;
        };
        let totals = if operation == "journal_sync" {
            &mut self.journal_sync
        } else {
            &mut self.directory_attr
        };
        totals.calls = totals.calls.saturating_add(1);
        totals.nanoseconds = totals.nanoseconds.saturating_add(duration);
        totals.maximum_nanoseconds = totals.maximum_nanoseconds.max(duration);
        totals.scanned_nodes = totals.scanned_nodes.saturating_add(scanned);
        totals.repeated_successful_syncs = totals
            .repeated_successful_syncs
            .saturating_add(u64::from(repeat && success));
        if repeat && success {
            totals.repeated_successful_sync_nanoseconds = totals
                .repeated_successful_sync_nanoseconds
                .saturating_add(duration);
        }
        if self.events.len() == self.capacity {
            self.events.pop_front();
            self.overwritten_events = self.overwritten_events.saturating_add(1);
        }
        self.events.push_back(Event {
            operation,
            started_ns,
            finished_ns,
            sequence,
            scanned_nodes: scanned,
            success,
            repeats_successful_sync: repeat,
        });
    }

    #[cfg(all(target_os = "linux", feature = "projection"))]
    pub(super) fn emit(&self) {
        // Only called after a clean, requested normal unmount. No per-operation
        // writes; output failure cannot change journal/session correctness.
        use std::io::{BufWriter, Write};
        let stderr = std::io::stderr().lock();
        let mut output = BufWriter::with_capacity(64 * 1024, stderr);
        if serde_json::to_writer(&mut output, self).is_ok() && output.write_all(b"\n").is_ok() {
            let _ = output.flush();
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn monotonic_ns() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a live output pointer and the fixed process-independent clock id.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return None;
    }
    u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn monotonic_ns() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_diagnostics_keep_totals_and_disclose_overwrite() {
        let mut d = Diagnostics::new(2);
        d.record("journal_sync", Some(10), Some(30), 1, 0, true);
        d.record("directory_attr", Some(31), Some(35), 1, 120, true);
        d.record("journal_sync", Some(36), Some(39), 1, 0, true);
        d.record("journal_sync", Some(40), Some(47), 2, 0, true);
        assert_eq!(d.events.len(), 2);
        assert_eq!(d.overwritten_events, 2);
        assert_eq!(d.journal_sync.calls, 3);
        assert_eq!(d.journal_sync.nanoseconds, 30);
        assert_eq!(d.journal_sync.repeated_successful_syncs, 1);
        assert_eq!(d.journal_sync.repeated_successful_sync_nanoseconds, 3);
        assert_eq!(d.directory_attr.scanned_nodes, 120);
        assert_eq!(d.directory_attr.calls, 1);
        assert!(d.events[0].repeats_successful_sync);
        assert!(!d.events[1].repeats_successful_sync);
        let encoded = serde_json::to_value(&d).unwrap();
        assert_eq!(encoded["capacity"], 2);
        assert!(encoded.get("last_successful_sync").is_none());
    }
}
