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
    cpu_ns: Option<u64>,
}

pub(super) struct Stamp {
    wall: Option<u64>,
    cpu: Option<u64>,
}

/// CPU endpoints are inside the wall endpoints. No clocks are read when the
/// caller has diagnostics disabled.
pub(super) fn start() -> Stamp {
    let wall = monotonic_ns();
    let cpu = thread_cpu_ns();
    Stamp { wall, cpu }
}

fn finish() -> Stamp {
    let cpu = thread_cpu_ns();
    let wall = monotonic_ns();
    Stamp { wall, cpu }
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
    cpu_clock: &'static str,
    cpu_clock_failures: u64,
    cpu_resolution_ns: Option<u64>,
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
        Some(RefCell::new(Self::new(
            CAPACITY,
            thread_cpu_resolution_ns(),
        )))
    }

    fn new(capacity: usize, cpu_resolution_ns: Option<u64>) -> Self {
        Self {
            schema: "sprig-writable-diagnostics-v2",
            clock: "CLOCK_MONOTONIC",
            capacity,
            overwritten_events: 0,
            clock_failures: 0,
            cpu_clock: "CLOCK_THREAD_CPUTIME_ID",
            cpu_clock_failures: u64::from(cpu_resolution_ns.is_none()),
            cpu_resolution_ns,
            directory_attr: Totals::default(),
            journal_sync: Totals::default(),
            events: VecDeque::with_capacity(capacity),
            last_successful_sync: None,
        }
    }

    pub(super) fn attr(&mut self, started: Stamp, sequence: u64, scanned: usize) {
        self.record(
            "directory_attr",
            started,
            finish(),
            sequence,
            scanned as u64,
            true,
        );
    }

    pub(super) fn sync(&mut self, started: Stamp, sequence: u64, success: bool) {
        self.record("journal_sync", started, finish(), sequence, 0, success);
    }

    fn record(
        &mut self,
        operation: &'static str,
        started: Stamp,
        finished: Stamp,
        sequence: u64,
        scanned: u64,
        success: bool,
    ) {
        let repeat = operation == "journal_sync" && self.last_successful_sync == Some(sequence);
        if operation == "journal_sync" {
            self.last_successful_sync = success.then_some(sequence);
        }
        let cpu_ns = started
            .cpu
            .zip(finished.cpu)
            .and_then(|(start, end)| end.checked_sub(start));
        if cpu_ns.is_none() {
            self.cpu_clock_failures = self.cpu_clock_failures.saturating_add(1);
        }
        let (Some(started_ns), Some(finished_ns)) = (started.wall, finished.wall) else {
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
            cpu_ns,
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
    read_clock(libc::CLOCK_MONOTONIC)
}

#[cfg(target_os = "linux")]
fn thread_cpu_ns() -> Option<u64> {
    read_clock(libc::CLOCK_THREAD_CPUTIME_ID)
}

#[cfg(target_os = "linux")]
fn read_clock(clock: libc::clockid_t) -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a live output pointer and a fixed supported clock id from above.
    if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
        return None;
    }
    timespec_ns(time)
}

#[cfg(target_os = "linux")]
fn thread_cpu_resolution_ns() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a live output pointer and the fixed current-thread CPU clock id.
    if unsafe { libc::clock_getres(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return None;
    }
    timespec_ns(time).filter(|resolution| *resolution > 0)
}

#[cfg(target_os = "linux")]
fn timespec_ns(time: libc::timespec) -> Option<u64> {
    if !(0..1_000_000_000).contains(&time.tv_nsec) {
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

#[cfg(not(target_os = "linux"))]
fn thread_cpu_ns() -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn thread_cpu_resolution_ns() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(wall: u64, cpu: u64) -> Stamp {
        Stamp {
            wall: Some(wall),
            cpu: Some(cpu),
        }
    }

    #[test]
    fn bounded_diagnostics_keep_totals_and_disclose_overwrite() {
        let mut d = Diagnostics::new(2, Some(1));
        d.record("journal_sync", stamp(10, 100), stamp(30, 110), 1, 0, true);
        d.record(
            "directory_attr",
            stamp(31, 110),
            stamp(35, 112),
            1,
            120,
            true,
        );
        d.record("journal_sync", stamp(36, 112), stamp(39, 113), 1, 0, true);
        d.record("journal_sync", stamp(40, 113), stamp(47, 115), 2, 0, true);
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
        assert_eq!(d.events[0].cpu_ns, Some(1));
        assert_eq!(d.events[1].cpu_ns, Some(2));
        assert_eq!(d.cpu_clock_failures, 0);
        let encoded = serde_json::to_value(&d).unwrap();
        assert_eq!(encoded["schema"], "sprig-writable-diagnostics-v2");
        assert_eq!(encoded["cpu_resolution_ns"], 1);
        assert_eq!(encoded["capacity"], 2);
        assert!(encoded.get("last_successful_sync").is_none());
    }

    #[test]
    fn cpu_failures_preserve_wall_events_and_do_not_fake_zero() {
        let mut d = Diagnostics::new(4, Some(1));
        d.record(
            "directory_attr",
            Stamp {
                wall: Some(1),
                cpu: None,
            },
            stamp(10, 100),
            1,
            20,
            true,
        );
        d.record("journal_sync", stamp(11, 110), stamp(15, 109), 1, 0, true);
        d.record("journal_sync", stamp(16, 120), stamp(20, 120), 1, 0, true);
        assert_eq!(d.events.len(), 3);
        assert_eq!(d.directory_attr.nanoseconds, 9);
        assert_eq!(d.journal_sync.nanoseconds, 8);
        assert_eq!(d.clock_failures, 0);
        assert_eq!(d.cpu_clock_failures, 2);
        assert_eq!(d.events[0].cpu_ns, None);
        assert_eq!(d.events[1].cpu_ns, None);
        assert_eq!(d.events[2].cpu_ns, Some(0));
        let encoded = serde_json::to_value(&d).unwrap();
        assert!(encoded["events"][0]["cpu_ns"].is_null());
        assert_eq!(encoded["events"][2]["cpu_ns"], 0);

        let unavailable = Diagnostics::new(1, None);
        assert_eq!(unavailable.cpu_clock_failures, 1);
        assert!(serde_json::to_value(unavailable).unwrap()["cpu_resolution_ns"].is_null());
    }

    #[test]
    fn invalid_wall_scopes_remain_visible_as_failures() {
        let mut d = Diagnostics::new(2, Some(1));
        d.record(
            "directory_attr",
            Stamp {
                wall: None,
                cpu: Some(100),
            },
            stamp(10, 101),
            1,
            20,
            true,
        );
        d.record("journal_sync", stamp(20, 102), stamp(19, 103), 1, 0, true);
        assert_eq!(d.clock_failures, 2);
        assert_eq!(d.cpu_clock_failures, 0);
        assert!(d.events.is_empty());
        assert_eq!(d.directory_attr.calls + d.journal_sync.calls, 0);
    }

    #[test]
    fn cpu_json_preserves_values_beyond_javascript_integer_precision() {
        let mut d = Diagnostics::new(1, Some(1));
        let elapsed = (1_u64 << 53) + 7;
        d.record(
            "journal_sync",
            stamp(1, 1),
            stamp(u64::MAX, elapsed + 1),
            1,
            0,
            true,
        );
        let text = serde_json::to_string(&d).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded["events"][0]["cpu_ns"].as_u64(), Some(elapsed));
        assert_eq!(decoded["events"][0]["finished_ns"].as_u64(), Some(u64::MAX));
    }

    #[test]
    fn maximum_width_dump_fits_sixteen_mib_without_an_output_allocation() {
        use std::io::{self, Write};

        #[derive(Default)]
        struct CountingWriter(usize);
        impl Write for CountingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        fn widest_totals() -> Totals {
            Totals {
                calls: u64::MAX,
                nanoseconds: u64::MAX,
                maximum_nanoseconds: u64::MAX,
                scanned_nodes: u64::MAX,
                repeated_successful_syncs: u64::MAX,
                repeated_successful_sync_nanoseconds: u64::MAX,
            }
        }

        // These deliberately inconsistent maxima are a serialization upper
        // bound, not a valid trace or a simulated filesystem operation.
        let mut d = Diagnostics::new(CAPACITY, Some(u64::MAX));
        d.overwritten_events = u64::MAX;
        d.clock_failures = u64::MAX;
        d.cpu_clock_failures = u64::MAX;
        d.directory_attr = widest_totals();
        d.journal_sync = widest_totals();
        for _ in 0..CAPACITY {
            d.events.push_back(Event {
                operation: "directory_attr",
                started_ns: u64::MAX,
                finished_ns: u64::MAX,
                sequence: u64::MAX,
                scanned_nodes: u64::MAX,
                success: false,
                repeats_successful_sync: false,
                cpu_ns: Some(u64::MAX),
            });
        }
        let mut output = CountingWriter::default();
        serde_json::to_writer(&mut output, &d).unwrap();
        output.write_all(b"\n").unwrap();
        assert_eq!(output.0, 16_188_232);
        assert!(output.0 < 16 * 1024 * 1024);
        assert!(std::mem::size_of::<Event>() <= 80);
        assert_eq!(d.events.len(), CAPACITY);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn cpu_clocks_are_explicitly_unavailable_off_linux() {
        assert_eq!(thread_cpu_ns(), None);
        assert_eq!(thread_cpu_resolution_ns(), None);
    }
}
