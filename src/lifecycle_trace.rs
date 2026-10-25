//! Opt-in elapsed-wall diagnostics; never a durability or readiness proof.
//! Nested spans overlap. Trace writes are charged to enclosing external timers.

use std::io::Write;
use std::time::Instant;

pub(crate) struct Span {
    phase: &'static str,
    started: Option<Instant>,
    retries: u64,
    ok: Option<bool>,
}

impl Span {
    pub(crate) fn new(phase: &'static str) -> Self {
        let enabled = std::env::var_os("SPRIG_LIFECYCLE_TRACE").is_some_and(|value| value == "1");
        Self {
            phase,
            started: enabled.then(Instant::now),
            retries: 0,
            ok: None,
        }
    }

    pub(crate) fn retry(&mut self) {
        self.retries += 1;
    }

    fn write(&self, output: &mut impl Write) -> std::io::Result<()> {
        if let Some(started) = self.started {
            let row = serde_json::json!({
                "version": 1,
                "pid": std::process::id(),
                "phase": self.phase,
                "elapsed_ns": started.elapsed().as_nanos(),
                "retries": self.retries,
                "ok": self.ok,
            });
            writeln!(output, "SPRIG_LIFECYCLE_TRACE {row}")?;
        }
        Ok(())
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if self.started.is_some() {
            // No panic or operation failure on a closed diagnostic output.
            // Scope-only spans have ok=null, not an inferred success.
            let _ = self.write(&mut std::io::stderr().lock());
        }
    }
}

pub(crate) fn measure<T, E>(
    phase: &'static str,
    operation: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    measure_counted(phase, |_| operation())
}

pub(crate) fn measure_counted<T, E>(
    phase: &'static str,
    operation: impl FnOnce(&mut u64) -> Result<T, E>,
) -> Result<T, E> {
    let mut span = Span::new(phase);
    let result = operation(&mut span.retries);
    span.ok = Some(result.is_ok());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_runs_once_and_preserves_success_and_error() {
        let mut calls = 0;
        let value: Result<_, &str> = measure("test", || {
            calls += 1;
            Ok(42)
        });
        assert_eq!(value, Ok(42));
        assert_eq!(calls, 1);
        let value: Result<(), _> = measure_counted("test", |retries| {
            *retries = 3;
            Err("original")
        });
        assert_eq!(value, Err("original"));
    }

    #[test]
    fn enabled_record_parses_and_disabled_record_is_absent() {
        let mut span = Span {
            phase: "test",
            started: Some(Instant::now()),
            retries: 2,
            ok: Some(false),
        };
        let mut output = Vec::new();
        span.write(&mut output).unwrap();
        let line = std::str::from_utf8(&output).unwrap();
        let row: serde_json::Value =
            serde_json::from_str(line.strip_prefix("SPRIG_LIFECYCLE_TRACE ").unwrap()).unwrap();
        assert_eq!(row["phase"], "test");
        assert_eq!(row["ok"], false);
        assert_eq!(row["retries"], 2);
        assert!(row["elapsed_ns"].is_u64());
        span.started = None;
        output.clear();
        span.write(&mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn broken_output_is_only_a_diagnostic_error() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut span = Span {
            phase: "test",
            started: Some(Instant::now()),
            retries: 0,
            ok: None,
        };
        assert_eq!(
            span.write(&mut Broken).unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        span.started = None;
    }
}
