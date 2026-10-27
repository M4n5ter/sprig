//! Operation-local two-file sync batches for native restore/checkout/recovery.
//! Own already-written handles; never reopen paths or finalize directories.
use std::fs::File;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

pub(crate) struct FileSync {
    senders: Vec<mpsc::SyncSender<File>>,
    results: Vec<mpsc::Receiver<io::Result<()>>>,
    workers: Vec<JoinHandle<()>>,
    pending: usize,
}

impl FileSync {
    pub(crate) fn new() -> io::Result<Self> {
        Self::start(File::sync_all)
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn for_test(
        sync: impl Fn(&File) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::start(sync)
    }

    // Private callback seam lets tests inject actual worker failures without
    // environment variables or a production fault-injection interface.
    fn start(sync: impl Fn(&File) -> io::Result<()> + Send + Sync + 'static) -> io::Result<Self> {
        let sync = Arc::new(sync);
        // Own partial construction before spawning; Drop joins on spawn error.
        let mut pool = Self {
            senders: Vec::with_capacity(2),
            results: Vec::with_capacity(2),
            workers: Vec::with_capacity(2),
            pending: 0,
        };
        for _ in 0..2 {
            let (send, receive) = mpsc::sync_channel::<File>(1);
            let (complete, result) = mpsc::sync_channel(1);
            let sync = Arc::clone(&sync);
            let worker = std::thread::Builder::new().spawn(move || {
                while let Ok(file) = receive.recv() {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sync(&file)))
                            .unwrap_or_else(|_| {
                                Err(io::Error::other("restore file sync worker panicked"))
                            });
                    drop(file);
                    if complete.send(result).is_err() {
                        break;
                    }
                }
            })?;
            pool.senders.push(send);
            pool.results.push(result);
            pool.workers.push(worker);
        }
        Ok(pool)
    }

    // At most two submitted jobs; the coordinator can hold a third newly
    // prepared handle while waiting for this batch to drain.
    pub(crate) fn submit(&mut self, file: File) -> io::Result<()> {
        if self.pending == 2 {
            self.drain()?;
        }
        self.senders[self.pending]
            .send(file)
            .map_err(|_| io::Error::other("restore file sync worker disconnected"))?;
        self.pending += 1;
        Ok(())
    }

    pub(crate) fn drain(&mut self) -> io::Result<()> {
        let mut first_error = None;
        // Submission order, not completion order; always wait for both jobs.
        for result in &self.results[..self.pending] {
            let result = result
                .recv()
                .unwrap_or_else(|_| Err(io::Error::other("restore file sync worker disconnected")));
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        self.pending = 0;
        first_error.map_or(Ok(()), Err)
    }

    fn join(&mut self) -> io::Result<()> {
        self.senders.clear();
        let mut first_error = None;
        for worker in self.workers.drain(..) {
            if worker.join().is_err() && first_error.is_none() {
                first_error = Some(io::Error::other("restore file sync worker panicked"));
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub(crate) fn finish(mut self, traversal: crate::Result<()>) -> crate::Result<()> {
        let completion = self.drain();
        let joined = self.join();
        // Earlier submitted work takes precedence over a later traversal error.
        completion?;
        joined?;
        traversal
    }
}

impl Drop for FileSync {
    fn drop(&mut self) {
        // Covers unwinding and partial startup. At most one completion per
        // worker fits its channel even when the coordinator is returning early.
        let _ = self.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "sprig-restore-sync-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, index: usize) -> File {
            let path = self.0.join(index.to_string());
            std::fs::write(&path, vec![b'x'; index]).unwrap();
            File::options().read(true).write(true).open(path).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn success_syncs_every_handle_and_joins_bounded_workers() {
        let fixture = Fixture::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let barrier = Barrier::new(2);
        let (done, live, high) = (finished.clone(), active.clone(), peak.clone());
        let mut pool = FileSync::start(move |file| {
            let count = live.fetch_add(1, Ordering::SeqCst) + 1;
            high.fetch_max(count, Ordering::SeqCst);
            barrier.wait();
            let result = file.sync_all();
            live.fetch_sub(1, Ordering::SeqCst);
            done.fetch_add(1, Ordering::SeqCst);
            result
        })
        .unwrap();
        for index in 1..=6 {
            pool.submit(fixture.file(index)).unwrap();
            assert!(pool.pending <= 2);
        }
        pool.finish(Ok(())).unwrap();
        assert_eq!(finished.load(Ordering::SeqCst), 6);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&finished), 1, "workers must be joined");
    }

    #[test]
    fn sync_failure_drains_sibling_and_precedes_later_traversal_error() {
        let fixture = Fixture::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let done = finished.clone();
        let barrier = Barrier::new(2);
        let mut pool = FileSync::start(move |file| {
            barrier.wait();
            let result = if file.metadata()?.len() == 1 {
                Err(io::Error::other("first file sync failed"))
            } else {
                file.sync_all()
            };
            done.fetch_add(1, Ordering::SeqCst);
            result
        })
        .unwrap();
        pool.submit(fixture.file(1)).unwrap();
        pool.submit(fixture.file(2)).unwrap();
        let error = pool
            .finish(Err(io::Error::other("later read failed").into()))
            .unwrap_err();
        assert_eq!(error.to_string(), "first file sync failed");
        assert_eq!(finished.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&finished), 1);
    }

    #[test]
    fn early_traversal_failure_still_completes_successful_syncs() {
        let fixture = Fixture::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let done = finished.clone();
        let mut pool = FileSync::start(move |file| {
            file.sync_all()?;
            done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        pool.submit(fixture.file(1)).unwrap();
        pool.submit(fixture.file(2)).unwrap();
        let error = pool
            .finish(Err(io::Error::other("later read failed").into()))
            .unwrap_err();
        assert_eq!(error.to_string(), "later read failed");
        assert_eq!(finished.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&finished), 1);
    }

    #[test]
    fn dropping_owner_joins_already_submitted_work() {
        let fixture = Fixture::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let done = finished.clone();
        let mut pool = FileSync::start(move |file| {
            file.sync_all()?;
            done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        pool.submit(fixture.file(1)).unwrap();
        pool.submit(fixture.file(2)).unwrap();
        drop(pool);
        assert_eq!(finished.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&finished), 1);
    }

    #[test]
    fn worker_panic_is_an_error_and_other_worker_is_joined() {
        let fixture = Fixture::new();
        let finished = Arc::new(AtomicUsize::new(0));
        let done = finished.clone();
        let mut pool = FileSync::start(move |file| {
            assert_ne!(file.metadata()?.len(), 1, "injected worker panic");
            file.sync_all()?;
            done.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        pool.submit(fixture.file(1)).unwrap();
        pool.submit(fixture.file(2)).unwrap();
        assert!(
            pool.finish(Ok(()))
                .unwrap_err()
                .to_string()
                .contains("panicked")
        );
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&finished), 1);
    }
}
