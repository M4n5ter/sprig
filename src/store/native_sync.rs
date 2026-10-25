//! Operation-owned, bounded sibling synchronization. No file or directory
//! durability barrier is omitted. Native trees must still be quiescent.
use super::*;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

const WIDTH: usize = 4;
type Worker = Arc<dyn Fn(&Path, &platform::Metadata) -> Result<()> + Send + Sync>;
#[cfg(test)]
type DirectoryCheck = Arc<dyn Fn(&Path) + Send + Sync>;
struct Job {
    path: PathBuf,
    metadata: platform::Metadata,
}
struct WorkerSlot {
    send: mpsc::SyncSender<Job>,
    done: mpsc::Receiver<Result<()>>,
}
struct Pool {
    slots: Vec<WorkerSlot>,
    handles: Vec<JoinHandle<()>>,
    pending: Vec<Job>,
    synced: SyncedInodes,
    worker: Worker,
    #[cfg(test)]
    directory_check: Option<DirectoryCheck>,
}
fn identity(m: &platform::Metadata) -> (u64, u128) {
    (m.dev(), platform::inode(m))
}
fn fingerprint(m: &platform::Metadata) -> (u64, i64, i64, i64, i64) {
    (
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}
impl Pool {
    fn new(worker: Worker) -> Self {
        Self {
            slots: Vec::new(),
            handles: Vec::new(),
            pending: Vec::with_capacity(WIDTH),
            synced: BTreeMap::new(),
            worker,
            #[cfg(test)]
            directory_check: None,
        }
    }
    fn start(&mut self, fail_after: Option<usize>) -> Result<()> {
        if !self.slots.is_empty() {
            return Ok(());
        }
        for index in 0..WIDTH {
            if fail_after == Some(index) {
                return Err(failure("injected sync worker startup failure"));
            }
            let (send, jobs) = mpsc::sync_channel::<Job>(1);
            let (results, done) = mpsc::sync_channel(1);
            let worker = self.worker.clone();
            let handle = std::thread::Builder::new()
                .name("sprig-sync".into())
                .spawn(move || {
                    while let Ok(job) = jobs.recv() {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            worker(&job.path, &job.metadata)
                        }))
                        .unwrap_or_else(|_| Err(failure("native sync worker panicked")));
                        if results.send(result).is_err() {
                            break;
                        }
                    }
                })?;
            // Own every successful spawn before another fallible spawn.
            self.handles.push(handle);
            self.slots.push(WorkerSlot { send, done });
        }
        Ok(())
    }
    fn drain(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // Narrow/deep trees avoid worker startup and channel round trips.
        if self.pending.len() == 1 {
            let job = self.pending.pop().unwrap();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (self.worker)(&job.path, &job.metadata)
            }))
            .unwrap_or_else(|_| Err(failure("native sync worker panicked")))?;
            self.synced
                .insert(identity(&job.metadata), fingerprint(&job.metadata));
            return Ok(());
        }
        if let Err(error) = self.start(None) {
            self.pending.clear();
            return Err(error);
        }
        let mut submitted = 0;
        let mut first_error = None;
        let mut submission_error = None;
        let mut keys = [(0, 0); WIDTH];
        let mut fingerprints = [(0, 0, 0, 0, 0); WIDTH];
        for (index, job) in self.pending.drain(..).enumerate() {
            keys[index] = identity(&job.metadata);
            fingerprints[index] = fingerprint(&job.metadata);
            if self.slots[index].send.send(job).is_err() {
                submission_error = Some(failure("native sync worker disconnected"));
                break;
            }
            submitted += 1;
        }
        // Receive in submission order, and consume every result even on error.
        // No result-vector allocation or sorting is needed.
        for index in 0..submitted {
            let result = self.slots[index]
                .done
                .recv()
                .unwrap_or_else(|_| Err(failure("native sync worker disconnected")));
            match result {
                Ok(()) => {
                    self.synced.insert(keys[index], fingerprints[index]);
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        first_error.or(submission_error).map_or(Ok(()), Err)
    }
    fn file(&mut self, path: PathBuf, metadata: platform::Metadata) -> Result<()> {
        let key = identity(&metadata);
        if self
            .pending
            .iter()
            .any(|job| identity(&job.metadata) == key)
        {
            self.drain()?;
            let current = platform::symlink_metadata(&path)?;
            return self.file(path, current);
        }
        if let Some(previous) = self.synced.get(&key) {
            if *previous != fingerprint(&metadata) {
                return Err(failure("linked file changed during synchronization"));
            }
            return Ok(());
        }
        if self.pending.len() == WIDTH {
            self.drain()?;
        }
        self.pending.push(Job { path, metadata });
        Ok(())
    }
    fn visit(&mut self, path: &Path) -> Result<()> {
        self.visit_owned(path.to_owned())
    }
    fn visit_owned(&mut self, path: PathBuf) -> Result<()> {
        let metadata = platform::symlink_metadata(&path)?;
        if metadata.is_dir() {
            self.drain()?;
            for entry in fs::read_dir(&path)? {
                self.visit_owned(entry?.path())?;
            }
            self.drain()?;
            #[cfg(test)]
            if let Some(check) = &self.directory_check {
                check(&path);
            }
            sync_directory(&path)?;
        } else if metadata.is_file() {
            self.file(path, metadata)?;
        } else if !metadata.file_type().is_symlink() {
            return Err(failure(
                "special file encountered while syncing native generation",
            ));
        }
        Ok(())
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.slots.clear();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}
pub(super) fn run(path: &Path) -> Result<()> {
    let mut pool = Pool::new(Arc::new(sync_verified_file));
    let traversal = pool.visit(path);
    let completion = pool.drain();
    completion.and(traversal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("sprig-sync-{}", new_id().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::write(&p, b"payload").unwrap();
            p
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn enqueue(pool: &mut Pool, path: &Path) {
        pool.file(path.to_owned(), platform::symlink_metadata(path).unwrap())
            .unwrap();
    }
    #[test]
    fn narrow_tree_never_starts_workers() {
        let f = Fixture::new();
        let mut p = Pool::new(Arc::new(sync_verified_file));
        for i in 0..20 {
            let dir = f.0.join(i.to_string());
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("a"), b"a").unwrap();
        }
        p.visit(&f.0).unwrap();
        assert!(p.slots.is_empty());
        assert_eq!(p.synced.len(), 20);
    }
    #[test]
    fn full_flush_and_hardlink_dedup() {
        let f = Fixture::new();
        let a = f.file("a");
        fs::hard_link(&a, f.0.join("b")).unwrap();
        for i in 0..20 {
            f.file(&format!("f{i}"));
        }
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let mut p = Pool::new(Arc::new(move |path, m| {
            sync_verified_file(path, m)?;
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        p.visit(&f.0).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 21);
        assert_eq!(p.synced.len(), 21);
        // Another checkpoint must flush every inode again, never trust old metadata.
        drop(p);
        let mut p = Pool::new(Arc::new(sync_verified_file));
        p.visit(&f.0).unwrap();
        assert_eq!(p.synced.len(), 21);
    }
    #[test]
    fn all_jobs_complete_and_first_submitted_error_wins() {
        let f = Fixture::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let mut p = Pool::new(Arc::new(move |path, _| {
            let name = path.file_name().unwrap().to_str().unwrap();
            if name == "first" {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            c.fetch_add(1, Ordering::SeqCst);
            Err(failure(name))
        }));
        enqueue(&mut p, &f.file("first"));
        enqueue(&mut p, &f.file("second"));
        assert_eq!(p.drain().unwrap_err().to_string(), "first");
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(p.pending.is_empty());
        assert!(p.synced.is_empty());
    }
    #[test]
    fn worker_panic_is_returned_and_all_results_drained() {
        let f = Fixture::new();
        let mut p = Pool::new(Arc::new(|_, _| panic!("injected")));
        enqueue(&mut p, &f.file("a"));
        enqueue(&mut p, &f.file("b"));
        assert!(p.drain().unwrap_err().to_string().contains("panicked"));
        assert!(p.pending.is_empty());
    }
    #[test]
    fn changed_pending_alias_is_restat_after_drain() {
        let f = Fixture::new();
        let a = f.file("a");
        let b = f.0.join("b");
        fs::hard_link(&a, &b).unwrap();
        let before = platform::symlink_metadata(&b).unwrap();
        let mut p = Pool::new(Arc::new(|path, m| {
            sync_verified_file(path, m)?;
            fs::write(path, b"changed longer")?;
            Ok(())
        }));
        enqueue(&mut p, &a);
        assert!(
            p.file(b, before)
                .unwrap_err()
                .to_string()
                .contains("linked file changed")
        );
    }
    #[test]
    fn file_mutation_rejected() {
        let f = Fixture::new();
        let a = f.file("a");
        let before = platform::symlink_metadata(&a).unwrap();
        fs::write(&a, b"changed").unwrap();
        assert!(
            sync_verified_file(&a, &before)
                .unwrap_err()
                .to_string()
                .contains("file changed")
        );
    }
    #[test]
    fn partial_startup_is_owned_and_joined() {
        let worker: Worker = Arc::new(sync_verified_file);
        let mut p = Pool::new(worker.clone());
        assert!(p.start(Some(1)).is_err());
        assert_eq!(p.handles.len(), 1);
        drop(p);
        assert_eq!(Arc::strong_count(&worker), 1);
    }
    #[test]
    fn serial_oracle_and_parallel_preserve_tree() {
        let f = Fixture::new();
        for i in 0..32 {
            f.file(&format!("f{i}"));
        }
        let sub = f.0.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a"), b"other").unwrap();
        sync_native_entries(&f.0, &mut BTreeMap::new()).unwrap();
        run(&f.0).unwrap();
        for i in 0..32 {
            assert_eq!(fs::read(f.0.join(format!("f{i}"))).unwrap(), b"payload");
        }
    }
    #[test]
    fn child_files_finish_before_directory_barrier() {
        let f = Fixture::new();
        let sub = f.0.join("sub");
        fs::create_dir(&sub).unwrap();
        for i in 0..12 {
            fs::write(sub.join(format!("f{i}")), b"a").unwrap();
        }
        let completed = Arc::new(AtomicUsize::new(0));
        let c = completed.clone();
        let mut p = Pool::new(Arc::new(move |path, m| {
            sync_verified_file(path, m)?;
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
        p.directory_check = Some(Arc::new(move |_| {
            assert_eq!(completed.load(Ordering::SeqCst), 12)
        }));
        p.visit(&f.0).unwrap();
    }
    #[test]
    fn earlier_worker_error_precedes_later_send_failure() {
        let f = Fixture::new();
        let mut p = Pool::new(Arc::new(|_, _| Err(failure("earlier"))));
        p.start(None).unwrap();
        let (send, recv) = mpsc::sync_channel(1);
        drop(recv);
        p.slots[1].send = send;
        enqueue(&mut p, &f.file("a"));
        enqueue(&mut p, &f.file("b"));
        assert_eq!(p.drain().unwrap_err().to_string(), "earlier");
        assert!(p.pending.is_empty());
    }
    #[test]
    fn no_directory_barrier_on_failed_child() {
        let f = Fixture::new();
        f.file("a");
        f.file("b");
        let mut p = Pool::new(Arc::new(|_, _| Err(failure("injected I/O failure"))));
        p.directory_check = Some(Arc::new(|_| panic!("directory barrier after failed child")));
        assert!(p.visit(&f.0).is_err());
    }
    #[test]
    fn sibling_work_is_parallel_and_bounded() {
        let f = Fixture::new();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let a = active.clone();
        let m = maximum.clone();
        let mut p = Pool::new(Arc::new(move |path, metadata| {
            let current = a.fetch_add(1, Ordering::SeqCst) + 1;
            m.fetch_max(current, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while m.load(Ordering::SeqCst) < WIDTH && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            let result = sync_verified_file(path, metadata);
            a.fetch_sub(1, Ordering::SeqCst);
            result
        }));
        for i in 0..WIDTH {
            enqueue(&mut p, &f.file(&format!("f{i}")));
        }
        p.drain().unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), WIDTH);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(p.handles.len(), WIDTH);
    }
    #[test]
    fn pending_error_precedes_later_traversal_error() {
        let f = Fixture::new();
        let mut p = Pool::new(Arc::new(|_, _| Err(failure("earlier pending file"))));
        enqueue(&mut p, &f.file("a"));
        let traversal = p.visit(&f.0.join("missing"));
        assert!(traversal.is_err());
        let completion = p.drain();
        assert_eq!(
            completion.and(traversal).unwrap_err().to_string(),
            "earlier pending file"
        );
    }
    #[test]
    fn every_checkpoint_keeps_full_file_and_directory_barrier_set() {
        let f = Fixture::new();
        let mut dirs = vec![f.0.clone()];
        for i in 0..133 {
            let dir = f.0.join(format!("d{i}"));
            fs::create_dir(&dir).unwrap();
            dirs.push(dir);
        }
        for i in 0..200 {
            fs::write(dirs[i % dirs.len()].join(format!("f{i}")), b"payload").unwrap();
        }
        for _ in 0..2 {
            let files = Arc::new(AtomicUsize::new(0));
            let directories = Arc::new(AtomicUsize::new(0));
            let c = files.clone();
            let d = directories.clone();
            let mut p = Pool::new(Arc::new(move |path, metadata| {
                sync_verified_file(path, metadata)?;
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));
            p.directory_check = Some(Arc::new(move |_| {
                d.fetch_add(1, Ordering::SeqCst);
            }));
            p.visit(&f.0).unwrap();
            assert_eq!(files.load(Ordering::SeqCst), 200);
            assert_eq!(directories.load(Ordering::SeqCst), 134);
        }
    }
}
