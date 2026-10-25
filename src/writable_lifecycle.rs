//! Root-only lifecycle for a direct owned-CAS FUSE mount, without OverlayFS.
//!
//! Shared projection helpers retain their strict path, lock, handshake and
//! normal-unmount policy. A writable mount has a generation-and-revision-bound
//! source name and only one target, `tree`. No xattr is needed as a control
//! channel, and no helper installation, lazy detach or PID-file signalling is
//! used. Stopping waits for the journal lease to close; callers still reopen,
//! replay and sync the core before publishing a revision.

use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::content::{self, Manifest, Result};
use crate::projection::lifecycle::{
    self as common, Control, Mount, MountState, checked_directory, checked_directory_chain,
    normal_unmount, read_mounts, require_root, unique_attempt, wait_ready,
};
use crate::projection::objects::invalid;
use crate::writable_fuse::WritableFuse;

const SOURCE_PREFIX: &str = "sprig-writable-cas:";

pub(crate) fn take_start_lock(generation: &Path) -> Result<File> {
    common::take_start_lock(generation)
}

fn generation_name(generation: &Path) -> io::Result<&str> {
    let name = generation
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("invalid writable generation name"))?;
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid("invalid writable generation name"));
    }
    Ok(name)
}

fn valid_revision(revision: &str) -> bool {
    revision.len() == 64
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn source(generation: &Path, revision: &str) -> io::Result<String> {
    if !valid_revision(revision) {
        return Err(invalid("invalid writable base revision"));
    }
    Ok(format!(
        "{SOURCE_PREFIX}{}:{revision}",
        generation_name(generation)?
    ))
}

fn validate_mounts(
    generation: &Path,
    mounts: &[Mount],
    revision: Option<&str>,
) -> io::Result<MountState> {
    let target = generation.join("tree");
    let prefix = format!("{SOURCE_PREFIX}{}:", generation_name(generation)?);
    let expected = revision
        .map(|revision| source(generation, revision))
        .transpose()?;
    let mut mounted = false;
    for mount in mounts
        .iter()
        .filter(|mount| mount.point.starts_with(generation))
    {
        let source_matches = if let Some(expected) = &expected {
            mount.source == *expected
        } else {
            mount
                .source
                .strip_prefix(&prefix)
                .is_some_and(valid_revision)
        };
        if mounted
            || mount.point != target
            || mount.root != Path::new("/")
            || mount.filesystem != "fuse"
            || !source_matches
            || !["rw", "nosuid", "nodev", "noatime"]
                .iter()
                .all(|option| mount.options.contains(*option))
            || mount.super_options.get("user_id").map(String::as_str) != Some("0")
            || mount.super_options.get("group_id").map(String::as_str) != Some("0")
            || !mount.super_options.contains_key("default_permissions")
            || ["allow_other", "allow_root"].iter().any(|option| {
                mount.options.contains(*option) || mount.super_options.contains_key(*option)
            })
        {
            return Err(invalid("unexpected writable CAS mount binding"));
        }
        mounted = true;
    }
    Ok(if mounted {
        MountState::Mounted
    } else {
        MountState::Unmounted
    })
}

pub(crate) fn inspect(generation: &Path) -> Result<MountState> {
    // Inspection also serves unmounted, non-root SDK workspaces. Mount/start
    // retain the stricter root-owned directory requirement.
    checked_directory_chain(generation)?;
    if !fs::symlink_metadata(generation)?.is_dir() {
        return Err(invalid("writable generation must be a directory").into());
    }
    Ok(validate_mounts(generation, &read_mounts()?, None)?)
}

pub(crate) fn verify_binding(generation: &Path, revision: &str) -> Result<()> {
    checked_directory(generation)?;
    if validate_mounts(generation, &read_mounts()?, Some(revision))? != MountState::Mounted {
        return Err(invalid("owned writable CAS workspace is not mounted").into());
    }
    let root = fs::symlink_metadata(generation.join("tree"))?;
    if !root.is_dir() || root.ino() != crate::writable::ROOT_INODE {
        return Err(invalid("writable CAS root does not match its inode binding").into());
    }
    Ok(())
}

/// The caller holds the inherited startup lock and an already-open core whose
/// journal binding has been verified against this revision and generation.
pub(crate) fn serve(filesystem: WritableFuse, generation: &Path, revision: &str) -> Result<()> {
    require_root()?;
    if inspect(generation)? != MountState::Unmounted {
        return Err(invalid("writable server refuses to stack over an existing mount").into());
    }
    let target = generation.join("tree");
    checked_directory(&target)?;
    if fs::read_dir(&target)?.next().is_some() {
        return Err(invalid("writable mountpoint must be empty").into());
    }
    let metadata = fs::symlink_metadata(&target)?;
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/dev/fuse")?;
    let data = CString::new(format!(
        "fd={},rootmode={:o},user_id=0,group_id=0,default_permissions,max_read=1048576",
        device.as_raw_fd(),
        metadata.mode()
    ))?;
    let source = CString::new(source(generation, revision)?)?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: live FUSE descriptor and terminated strings, fixed restrictive
    // flags, one direct attempt, and no userspace helper or fallback mount.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME,
            data.as_ptr().cast(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    // from_fd owns no Mount whose Drop could perform a lazy unmount.
    let mut session = fuser::Session::from_fd(filesystem, device.into(), fuser::SessionACL::Owner);
    session.run()?;
    Ok(())
}

pub(crate) fn start(
    store_root: &Path,
    revision: &str,
    generation: &Path,
    executable: &Path,
    manifest: &Manifest,
    initialize: bool,
) -> Result<()> {
    require_root()?;
    content::validate_manifest(manifest)?;
    let _ = source(generation, revision)?;
    checked_directory(store_root)?;
    checked_directory(generation)?;
    if generation != store_root.join("native").join(generation_name(generation)?) {
        return Err(invalid(
            "writable generation must be a direct child of this store's native directory",
        )
        .into());
    }
    let control = Control::acquire(generation)?;
    if inspect(generation)? == MountState::Mounted {
        verify_binding(generation, revision)?;
        return Ok(());
    }
    crate::writable_session::ensure_clean(generation)?;
    let target = generation.join("tree");
    if initialize {
        match DirBuilder::new().mode(0o700).create(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    checked_directory(&target)?;
    if fs::read_dir(&target)?.next().is_some() {
        return Err(invalid("unmounted writable target must be empty").into());
    }
    File::open(&target)?.sync_all()?;
    File::open(generation)?.sync_all()?;
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(generation.join(format!("writable-{}.log", unique_attempt())))?;
    let control_fd = control.0.as_raw_fd();
    let mut command = Command::new(executable);
    command
        .env("SPRIG_PROJECTION_START_LOCK_FD", control_fd.to_string())
        .current_dir(store_root)
        .arg("--store")
        .arg(store_root)
        .arg("__writable-serve")
        .arg("--revision")
        .arg(revision)
        .arg("--generation")
        .arg(generation_name(generation)?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: only async-signal-safe fcntl/setsid are invoked after fork. The
    // inherited open description keeps startup serialized after a timeout.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(control_fd, libc::F_SETFD, 0) < 0 || libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    if let Err(error) = wait_ready(&mut child) {
        // No absent-mount/PID inference and no signal: startup may still own
        // the inherited control lock. Keep all journal and log evidence.
        return Err(io::Error::other(format!("{error}; writable startup outcome may be uncertain; generation and server retained for inspection")).into());
    }
    // Any unexpected binding is retained, never unmounted as if it were ours.
    verify_binding(generation, revision)?;
    Ok(())
}

/// Normal unmount preserves EBUSY and never destroys dirty journal evidence.
pub(crate) fn stop(generation: &Path) -> Result<()> {
    require_root()?;
    let _control = Control::acquire(generation)?;
    let stopped = if inspect(generation)? == MountState::Mounted {
        let record = crate::writable_session::request_stop(generation)?;
        normal_unmount(&generation.join("tree"))?;
        Some(record)
    } else {
        None
    };
    if inspect(generation)? != MountState::Unmounted {
        return Err(invalid("writable CAS mount remains after normal unmount").into());
    }
    wait_journal_release(generation)?;
    if let Some(record) = stopped {
        crate::writable_session::finish_stop(generation, &record)?;
    }
    Ok(())
}

fn wait_journal_release(generation: &Path) -> io::Result<()> {
    checked_directory_chain(generation)?;
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(generation.join("writable.lease"))?;
    let metadata = lease.metadata()?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err(invalid("unsafe writable journal lease"));
    }
    let started = Instant::now();
    wait_for_journal_lease(
        || {
            // SAFETY: live owned regular-file descriptor. Only acquisition of
            // the actual exclusive lease proves the journal has been released.
            if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        },
        || started.elapsed(),
        std::thread::sleep,
    )
}

fn wait_for_journal_lease(
    mut acquire: impl FnMut() -> io::Result<()>,
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> io::Result<()> {
    let timeout = Duration::from_secs(15);
    let mut delay = Duration::from_millis(1);
    loop {
        match acquire() {
            Ok(()) => return Ok(()),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        // Preserve acquire-before-timeout semantics: expiry never substitutes
        // for lock ownership. Short initial sleeps avoid rounding prompt daemon
        // teardown up to 25 ms; persistent contention returns to that same cap.
        let remaining = timeout.saturating_sub(elapsed());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "writable server still holds its journal lease; generation retained",
            ));
        }
        // No spin, signals, force-unmount, lock deletion or deadline reset.
        // Clip the last sleep rather than deliberately sleeping past the budget.
        sleep(delay.min(remaining));
        delay = (delay * 2).min(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simulate_lease_wait(
        release_at: Option<Duration>,
        initial_errors: &[io::ErrorKind],
        oversleep: Duration,
    ) -> (io::Result<()>, Vec<Duration>, usize, Duration) {
        use std::cell::{Cell, RefCell};
        let now = Cell::new(Duration::ZERO);
        let attempts = Cell::new(0);
        let sleeps = RefCell::new(Vec::new());
        let mut errors = initial_errors.iter().copied();
        let result = wait_for_journal_lease(
            || {
                attempts.set(attempts.get() + 1);
                if let Some(error) = errors.next() {
                    return Err(error.into());
                }
                if release_at.is_some_and(|release| now.get() >= release) {
                    Ok(())
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                }
            },
            || now.get(),
            |delay| {
                sleeps.borrow_mut().push(delay);
                now.set(now.get() + delay + oversleep);
            },
        );
        (result, sleeps.into_inner(), attempts.get(), now.get())
    }

    #[test]
    fn lease_backoff_immediate_release_never_sleeps() {
        let (result, sleeps, attempts, elapsed) =
            simulate_lease_wait(Some(Duration::ZERO), &[], Duration::ZERO);
        result.unwrap();
        assert!(sleeps.is_empty());
        assert_eq!(attempts, 1);
        assert_eq!(elapsed, Duration::ZERO);
    }

    #[test]
    fn lease_backoff_prompt_release_requires_actual_acquisition() {
        let (result, sleeps, attempts, elapsed) =
            simulate_lease_wait(Some(Duration::from_millis(2)), &[], Duration::ZERO);
        result.unwrap();
        assert_eq!(sleeps, [Duration::from_millis(1), Duration::from_millis(2)]);
        assert_eq!(attempts, 3);
        assert_eq!(elapsed, Duration::from_millis(3));
    }

    #[test]
    fn lease_backoff_late_release_caps_sleep_and_keeps_original_deadline() {
        let (result, sleeps, attempts, elapsed) =
            simulate_lease_wait(Some(Duration::from_millis(14999)), &[], Duration::ZERO);
        result.unwrap();
        assert_eq!(
            &sleeps[..6],
            &[1, 2, 4, 8, 16, 25].map(Duration::from_millis)
        );
        assert!(
            sleeps
                .iter()
                .all(|delay| !delay.is_zero() && *delay <= Duration::from_millis(25))
        );
        assert_eq!(elapsed, Duration::from_secs(15));
        assert_eq!(attempts, sleeps.len() + 1);
    }

    #[test]
    fn lease_backoff_never_release_times_out_without_oversleep_or_spin() {
        let (result, sleeps, attempts, elapsed) = simulate_lease_wait(None, &[], Duration::ZERO);
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "writable server still holds its journal lease; generation retained"
        );
        assert_eq!(elapsed, Duration::from_secs(15));
        assert_eq!(attempts, sleeps.len() + 1);
        assert!(sleeps.len() < 610);
        assert_eq!(sleeps.last(), Some(&Duration::from_millis(19)));
        assert!(
            sleeps
                .iter()
                .all(|delay| !delay.is_zero() && *delay <= Duration::from_millis(25))
        );
    }

    #[test]
    fn lease_backoff_interrupted_acquisition_sleeps_and_rechecks_lock() {
        let (result, sleeps, attempts, _) = simulate_lease_wait(
            Some(Duration::ZERO),
            &[io::ErrorKind::Interrupted, io::ErrorKind::Interrupted],
            Duration::ZERO,
        );
        result.unwrap();
        assert_eq!(sleeps, [Duration::from_millis(1), Duration::from_millis(2)]);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn lease_backoff_fatal_error_is_returned_unchanged_without_sleep() {
        let result = wait_for_journal_lease(
            || Err(io::Error::from_raw_os_error(libc::EIO)),
            || panic!("fatal errors must not consult timeout"),
            |_| panic!("fatal errors must not sleep"),
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn lease_backoff_preserves_acquisition_before_timeout_after_scheduler_delay() {
        let (result, sleeps, attempts, elapsed) =
            simulate_lease_wait(Some(Duration::from_millis(1)), &[], Duration::from_secs(16));
        result.unwrap();
        assert_eq!(sleeps, [Duration::from_millis(1)]);
        assert_eq!(attempts, 2);
        assert!(elapsed > Duration::from_secs(15));
    }

    #[test]
    fn lease_backoff_scheduler_delay_does_not_reset_timeout() {
        let (result, sleeps, attempts, elapsed) =
            simulate_lease_wait(None, &[], Duration::from_secs(16));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(sleeps, [Duration::from_millis(1)]);
        assert_eq!(attempts, 2);
        assert!(elapsed > Duration::from_secs(15));
    }

    #[test]
    fn writable_binding_requires_one_exact_restrictive_mount() {
        let generation = Path::new("/store/native/g");
        let revision = "a".repeat(64);
        let make_mount = || Mount {
            root: "/".into(),
            point: generation.join("tree"),
            options: ["rw", "nosuid", "nodev", "noatime"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            filesystem: "fuse".into(),
            source: source(generation, &revision).unwrap(),
            super_options: [
                ("user_id", "0"),
                ("group_id", "0"),
                ("default_permissions", ""),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        };
        assert_eq!(
            validate_mounts(generation, &[], Some(&revision)).unwrap(),
            MountState::Unmounted
        );
        assert_eq!(
            validate_mounts(generation, &[make_mount()], Some(&revision)).unwrap(),
            MountState::Mounted
        );
        assert!(validate_mounts(generation, &[make_mount()], Some(&"b".repeat(64))).is_err());
        assert!(validate_mounts(generation, &[make_mount(), make_mount()], None).is_err());
        for option in ["rw", "nosuid", "nodev", "noatime"] {
            let mut mount = make_mount();
            mount.options.remove(option);
            assert!(validate_mounts(generation, &[mount], None).is_err());
        }
        let mut mount = make_mount();
        mount.super_options.remove("default_permissions");
        assert!(validate_mounts(generation, &[mount], None).is_err());
        let mut mount = make_mount();
        mount
            .super_options
            .insert("allow_other".into(), String::new());
        assert!(validate_mounts(generation, &[mount], None).is_err());
        let mut mount = make_mount();
        mount.point = generation.join("tree/nested");
        assert!(validate_mounts(generation, &[mount], None).is_err());
        let mut mount = make_mount();
        mount.source = source(Path::new("/store/native/other"), &revision).unwrap();
        assert!(validate_mounts(generation, &[mount], None).is_err());
    }
}
