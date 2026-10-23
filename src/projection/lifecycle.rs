//! Explicit, root-only mount lifecycle. No helper installation, forced detach,
//! lazy unmount, PID-file signalling, or deletion of retained upper data.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::objects::invalid;
use crate::content::{self, Manifest, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountState {
    Unmounted,
    Partial,
    Mounted,
}

pub(crate) struct Control(pub(crate) File);
impl Control {
    pub(crate) fn acquire(generation: &Path) -> io::Result<Self> {
        checked_directory(generation)?;
        if fs::symlink_metadata(generation)?.mode() & 0o077 != 0 {
            return Err(invalid(
                "projection generation must be private to its owner",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(generation.join("projection.lock"))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
            return Err(invalid("unsafe projection control lock"));
        }
        // SAFETY: file owns a live descriptor. Nonblocking prevents hidden waits.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(file))
    }
}
// Closing, rather than explicitly unlocking, preserves the same flock while
// its inherited server descriptor still owns startup after a timeout.

/// Adopts only the verified lock descriptor deliberately inherited by start.
/// Keep it until the ready callback finishes or the server exits on failure.
pub fn take_start_lock(generation: &Path) -> Result<File> {
    checked_directory(generation)?;
    let descriptor: i32 = std::env::var("SPRIG_PROJECTION_START_LOCK_FD")
        .map_err(|_| invalid("projection server requires an inherited startup lock"))?
        .parse()
        .map_err(|_| invalid("invalid startup lock descriptor"))?;
    if descriptor < 3 {
        return Err(invalid("unsafe startup lock descriptor").into());
    }
    let expected = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(generation.join("projection.lock"))?
        .metadata()?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to a correctly sized writable stat buffer. No
    // ownership is taken until the kernel confirms the expected regular inode.
    if unsafe { libc::fstat(descriptor, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: successful fstat initialized every field used below.
    let stat = unsafe { stat.assume_init() };
    if !expected.is_file()
        || stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_dev != expected.dev()
        || stat.st_ino != expected.ino()
        || stat.st_uid != 0
        || stat.st_mode & 0o077 != 0
    {
        return Err(invalid("startup lock does not match this generation").into());
    }
    // SAFETY: the verified inherited descriptor is owned exactly once here.
    let lock = unsafe { File::from_raw_fd(descriptor) };
    // SAFETY: restore close-on-exec and require exclusive lock ownership. For
    // the inherited open description this retains the parent's existing lock.
    if unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0
        || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(lock)
}

/// Runs one direct lower mount in the dedicated server process. The caller
/// must hold the inherited startup lock until the FUSE ready callback. Session
/// teardown closes its descriptor only; normal stop owns all unmounts.
pub fn serve(filesystem: super::ReadOnlyFuse, generation: &Path) -> Result<()> {
    require_root()?;
    if inspect(generation)? != MountState::Unmounted {
        return Err(invalid("server refuses to stack over an existing mount").into());
    }
    let target = generation.join("lower");
    checked_directory_chain(&target)?;
    if fs::read_dir(&target)?.next().is_some() {
        return Err(invalid("lower mountpoint must be empty").into());
    }
    let metadata = fs::symlink_metadata(&target)?;
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open("/dev/fuse")?;
    let data = CString::new(format!(
        "fd={},rootmode={:o},user_id=0,group_id=0,default_permissions,max_read={}",
        device.as_raw_fd(),
        metadata.mode(),
        super::MAX_READ_BYTES
    ))?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: live device descriptor and terminated string arguments. There is
    // exactly one direct attempt with fixed flags, no helper or retry fallback.
    if unsafe {
        libc::mount(
            c"sprig-cas".as_ptr(),
            target.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME,
            data.as_ptr().cast(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    // from_fd owns no fuser Mount object, whose Drop would otherwise lazily
    // detach. An interrupted server leaves the exact mount for normal stop.
    let mut session = fuser::Session::from_fd(filesystem, device.into(), fuser::SessionACL::Owner);
    session.run()?;
    Ok(())
}

/// Starts an exact retained generation. Existing upper/work contents are never
/// removed or rebased; the caller must hold its store and workspace leases.
/// Only an unpublished fresh generation may set initialize=true. Remounting a
/// retained generation fails closed if any required directory is missing.
pub fn start(
    store_root: &Path,
    revision: &str,
    generation: &Path,
    executable: &Path,
    manifest: &Manifest,
    initialize: bool,
) -> Result<()> {
    require_root()?;
    content::validate_manifest(manifest)?;
    checked_directory(store_root)?;
    checked_directory(generation)?;
    let name = generation
        .file_name()
        .ok_or_else(|| invalid("generation has no name"))?;
    if generation != store_root.join("native").join(name) {
        return Err(
            invalid("generation must be a direct child of this store's native directory").into(),
        );
    }
    let control = Control::acquire(generation)?;
    if !initialize {
        for name in ["upper", "work", "lower", "tree"] {
            checked_directory_chain(&generation.join(name))?;
        }
    }
    match inspect(generation)? {
        MountState::Mounted => {
            verify_binding(generation, manifest)?;
            return Ok(());
        }
        MountState::Partial => {
            return Err(
                invalid("partial projection mount; stop normally before restarting").into(),
            );
        }
        MountState::Unmounted => {}
    }
    let upper_path = generation.join("upper");
    let upper_exists = match fs::symlink_metadata(&upper_path) {
        Ok(_) => {
            checked_directory_chain(&upper_path)?;
            if fs::symlink_metadata(&upper_path)?.uid() != 0 {
                return Err(invalid("projection upper must remain root-owned").into());
            }
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && initialize => false,
        Err(error) => return Err(error.into()),
    };
    for name in ["lower", "work", "tree"] {
        let path = generation.join(name);
        if !initialize {
            checked_directory_chain(&path)?;
            if fs::symlink_metadata(&path)?.uid() != 0 {
                return Err(invalid("projection directory must remain root-owned").into());
            }
            continue;
        }
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                checked_directory_chain(&path)?;
                if fs::symlink_metadata(&path)?.uid() != 0 {
                    return Err(invalid("projection directory must remain root-owned").into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    for name in ["lower", "tree"] {
        if fs::read_dir(generation.join(name))?.next().is_some() {
            return Err(invalid("unmounted projection targets must be empty").into());
        }
    }
    if !upper_exists {
        // Publish the initial root metadata atomically. A failed initialization
        // leaves only an unreferenced staging directory, never a partial upper.
        let pending = generation.join(format!("upper-init-{}", unique_attempt()));
        // OverlayFS index=off may split lower hardlinks during copy-up. Seed
        // complete multi-name groups into this fresh private upper atomically;
        // all other payloads remain lazy. Never reseed an existing upper.
        let mut counts = std::collections::BTreeMap::new();
        for entry in manifest.values() {
            if let Some(group) = &entry.link_group {
                *counts.entry(group).or_insert(0usize) += 1;
            }
        }
        let mut initial = Manifest::from([(String::new(), manifest[""].clone())]);
        for (path, entry) in manifest {
            if !entry
                .link_group
                .as_ref()
                .is_some_and(|group| counts[group] > 1)
            {
                continue;
            }
            initial.insert(path.clone(), entry.clone());
            let mut parent = path.as_str();
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                initial.insert(prefix.to_owned(), manifest[prefix].clone());
                parent = prefix;
            }
        }
        content::materialize(&initial, &store_root.join("objects"), &pending)?;
        fs::rename(&pending, &upper_path)?;
        File::open(generation)?.sync_all()?;
    }
    for name in ["upper", "work", "lower", "tree"] {
        File::open(generation.join(name))?.sync_all()?;
    }
    File::open(generation)?.sync_all()?;
    // Each attempt gets a fresh log; never truncate a prior failure's evidence.
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(generation.join(format!("projection-{}.log", unique_attempt())))?;
    let name = generation
        .file_name()
        .ok_or_else(|| invalid("generation has no name"))?;
    let control_fd = control.0.as_raw_fd();
    let mut command = Command::new(executable);
    command.env("SPRIG_PROJECTION_START_LOCK_FD", control_fd.to_string());
    // A persistent server must not pin a caller's old overlay as its cwd.
    command.current_dir(store_root);
    command
        .arg("--store")
        .arg(store_root)
        .arg("__projection-serve")
        .arg("--revision")
        .arg(revision)
        .arg("--generation")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: the pre-exec hook invokes only async-signal-safe fcntl/setsid
    // and constructs an OS error on failure. No locks or allocations are used.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(control_fd, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    if let Err(error) = wait_ready(&mut child) {
        // An unready server may still be mounting. Closing its readiness pipe
        // makes any eventual init fail. Its inherited startup lock remains
        // held until callback failure or process exit, preventing a racing
        // restart. Never signal it based on a racy mountinfo absence check. Retain this generation for explicit recovery.
        return Err(io::Error::other(format!("{error}; startup outcome may be uncertain; generation and server retained for inspection")).into());
    }
    if inspect(generation)? != MountState::Partial || !has_lower(generation)? {
        cleanup_failed_child(&mut child, generation);
        return Err(invalid("FUSE ready signal did not establish the exact lower mount").into());
    }
    if let Err(error) = verify_origin(generation, manifest) {
        cleanup_failed_child(&mut child, generation);
        return Err(error);
    }
    let options = format!(
        "lowerdir={},upperdir={},workdir={},redirect_dir=on,index=off,metacopy=off,xino=off,nfs_export=off",
        generation.join("lower").display(),
        generation.join("upper").display(),
        generation.join("work").display()
    );
    let target = CString::new(generation.join("tree").as_os_str().as_bytes())?;
    let options = CString::new(options)?;
    // SAFETY: all pointers are live NUL-terminated strings, flags are fixed, and
    // trusted generation paths have no mount-option separators.
    if unsafe {
        libc::mount(
            c"overlay".as_ptr(),
            target.as_ptr(),
            c"overlay".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        cleanup_failed_child(&mut child, generation);
        return Err(error.into());
    }
    if inspect(generation)? != MountState::Mounted {
        return Err(invalid(
            "projection mount did not establish its exact binding; retained for normal cleanup",
        )
        .into());
    }
    verify_binding(generation, manifest)?;
    // The child intentionally survives this command; no persisted PID is ever
    // used as authority to signal another process.
    Ok(())
}

/// Normal unmount only. On EBUSY, retained data and metadata are untouched. If
/// lower unmount fails after overlay succeeds, inspection reports Partial.
pub fn stop(generation: &Path) -> Result<()> {
    require_root()?;
    let _control = Control::acquire(generation)?;
    let mounts = read_mounts()?;
    validate_mounts(generation, &mounts)?;
    for name in ["tree", "lower"] {
        let target = generation.join(name);
        if mounts.iter().any(|mount| mount.point == target) {
            normal_unmount(&target)?;
        }
    }
    if inspect(generation)? != MountState::Unmounted {
        return Err(invalid("projection mount remains after normal unmount").into());
    }
    Ok(())
}

pub fn inspect(generation: &Path) -> Result<MountState> {
    checked_directory(generation)?;
    Ok(validate_mounts(generation, &read_mounts()?)?)
}

/// Verifies both mount topology and immutable content identity before a caller
/// treats the live tree as workspace contents. Disconnected FUSE reads fail.
pub fn verify_binding(generation: &Path, manifest: &Manifest) -> Result<()> {
    if inspect(generation)? != MountState::Mounted {
        return Err(invalid("workspace projection is not fully mounted").into());
    }
    verify_origin(generation, manifest)?;
    let metadata = fs::symlink_metadata(generation.join("tree"))?;
    if !metadata.is_dir() {
        return Err(invalid("projection tree is not a directory").into());
    }
    Ok(())
}

/// Returns the live lower's fresh server identity. Inode observations may only
/// be reused while this value is unchanged, never merely by matching revision.
pub fn incarnation(generation: &Path) -> Result<String> {
    if inspect(generation)? != MountState::Mounted {
        return Err(invalid("workspace projection is not fully mounted").into());
    }
    let lower = CString::new(generation.join("lower").as_os_str().as_bytes())?;
    let name = CString::new(super::SESSION_XATTR)?;
    let mut bytes = [0_u8; 65];
    // SAFETY: both strings are terminated and the writable buffer is sized as
    // declared. Exact lower mount topology was validated above.
    let count = unsafe {
        libc::lgetxattr(
            lower.as_ptr(),
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if count < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if count != 64
        || !bytes[..64]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(invalid("invalid projection session identity").into());
    }
    Ok(String::from_utf8(bytes[..64].to_vec())?)
}

fn verify_origin(generation: &Path, manifest: &Manifest) -> Result<()> {
    let lower = CString::new(generation.join("lower").as_os_str().as_bytes())?;
    let name = CString::new(super::ORIGIN_XATTR)?;
    let mut bytes = [0_u8; 256];
    // SAFETY: both strings are terminated and the writable buffer has its
    // declared length. Exact mount binding was checked before this read.
    let count = unsafe {
        libc::lgetxattr(
            lower.as_ptr(),
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if count < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let actual: (String, String) = serde_json::from_slice(&bytes[..count as usize])?;
    let digest = blake3::hash(&serde_json::to_vec(manifest)?)
        .to_hex()
        .to_string();
    if actual != (digest, String::new()) {
        return Err(
            invalid("mounted lower does not match the requested immutable manifest").into(),
        );
    }
    Ok(())
}

pub(crate) fn require_root() -> io::Result<()> {
    // SAFETY: geteuid has no memory preconditions or side effects.
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "projection mounts require explicit root execution; no helper fallback",
        ));
    }
    Ok(())
}

pub(crate) fn checked_directory(path: &Path) -> io::Result<()> {
    checked_directory_chain(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(invalid(
            "projection directory must be root-owned and not group/world writable",
        ));
    }
    Ok(())
}

pub(crate) fn checked_directory_chain(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .as_os_str()
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b':' | b',' | b'\\' | 0))
    {
        return Err(invalid(
            "projection paths must be absolute and contain no mount-option separators",
        ));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(invalid("parent traversal in projection path"));
        }
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid(
                "projection path contains a symlink or non-directory",
            ));
        }
    }

    Ok(())
}

pub(crate) fn wait_ready(child: &mut Child) -> io::Result<()> {
    let mut pipe = child
        .stdout
        .take()
        .ok_or_else(|| invalid("missing readiness pipe"))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "projection server exited before readiness: {status}"
            )));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "projection readiness timed out",
            ));
        }
        let mut poll = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd and bounded nonnegative timeout.
        let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(250) as i32) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result > 0 {
            let mut byte = [0];
            pipe.read_exact(&mut byte)?;
            return if byte == *b"R" {
                Ok(())
            } else {
                Err(invalid("invalid projection readiness signal"))
            };
        }
    }
}

pub(crate) fn normal_unmount(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid("NUL mount path"))?;
    // SAFETY: a live terminated target; zero flags never force or lazily detach.
    if unsafe { libc::umount2(path.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn cleanup_failed_child(child: &mut Child, generation: &Path) {
    // Only a verified lower-only binding can be detached. Unknown mounts and
    // overlays retain their server, so cleanup cannot disconnect a live view.
    if matches!(inspect(generation), Ok(MountState::Partial))
        && matches!(has_lower(generation), Ok(true))
    {
        let _ = normal_unmount(&generation.join("lower"));
    }
    if matches!(inspect(generation), Ok(MountState::Unmounted)) {
        // Child is the original, unreaped process handle, never a stored PID.
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

pub(crate) fn unique_attempt() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

#[derive(Debug)]
pub(crate) struct Mount {
    pub(crate) root: PathBuf,
    pub(crate) point: PathBuf,
    pub(crate) options: BTreeSet<String>,
    pub(crate) filesystem: String,
    pub(crate) source: String,
    pub(crate) super_options: BTreeMap<String, String>,
}

pub(crate) fn read_mounts() -> io::Result<Vec<Mount>> {
    parse_mountinfo(&fs::read_to_string("/proc/self/mountinfo")?)
}

fn has_lower(generation: &Path) -> io::Result<bool> {
    let mounts = read_mounts()?;
    validate_mounts(generation, &mounts)?;
    Ok(mounts
        .iter()
        .any(|mount| mount.point == generation.join("lower")))
}

fn validate_mounts(generation: &Path, mounts: &[Mount]) -> io::Result<MountState> {
    let lower = generation.join("lower");
    let tree = generation.join("tree");
    let mut lower_present = false;
    let mut tree_present = false;
    for mount in mounts
        .iter()
        .filter(|mount| mount.point.starts_with(generation))
    {
        if mount.point == lower {
            if lower_present
                || mount.root != Path::new("/")
                || mount.filesystem != "fuse"
                || mount.source != "sprig-cas"
                || !["ro", "nosuid", "nodev"]
                    .iter()
                    .all(|option| mount.options.contains(*option))
                || mount.super_options.get("user_id").map(String::as_str) != Some("0")
                || mount.super_options.get("group_id").map(String::as_str) != Some("0")
                || ["allow_other", "allow_root"].iter().any(|option| {
                    mount.super_options.contains_key(*option) || mount.options.contains(*option)
                })
            {
                return Err(invalid("unexpected lower mount binding"));
            }
            lower_present = true;
        } else if mount.point == tree {
            let options = &mount.super_options;
            if tree_present
                || mount.root != Path::new("/")
                || mount.filesystem != "overlay"
                || mount.source != "overlay"
                || !["rw", "nosuid", "nodev"]
                    .iter()
                    .all(|option| mount.options.contains(*option))
                || options.get("lowerdir").map(Path::new) != Some(lower.as_path())
                || options.get("upperdir").map(Path::new)
                    != Some(generation.join("upper").as_path())
                || options.get("workdir").map(Path::new) != Some(generation.join("work").as_path())
                || ["userxattr", "volatile", "lowerdir+", "datadir+"]
                    .iter()
                    .any(|key| options.contains_key(*key))
                || ["metacopy", "index", "xino", "nfs_export"]
                    .iter()
                    .any(|key| options.get(*key).is_some_and(|value| value != "off"))
                || options
                    .get("redirect_dir")
                    .is_some_and(|value| value != "on")
            {
                return Err(invalid("unexpected overlay mount binding"));
            }
            tree_present = true;
        } else {
            return Err(invalid("unexpected nested mount in projection generation"));
        }
    }
    Ok(match (lower_present, tree_present) {
        (false, false) => MountState::Unmounted,
        (true, true) => MountState::Mounted,
        _ => MountState::Partial,
    })
}

fn parse_mountinfo(input: &str) -> io::Result<Vec<Mount>> {
    input
        .lines()
        .map(|line| {
            let (left, right) = line
                .split_once(" - ")
                .ok_or_else(|| invalid("invalid mountinfo separator"))?;
            let left: Vec<_> = left.split_ascii_whitespace().collect();
            let right: Vec<_> = right.split_ascii_whitespace().collect();
            if left.len() < 6 || right.len() != 3 {
                return Err(invalid("invalid mountinfo fields"));
            }
            let mut options = BTreeMap::new();
            for option in right[2].split(',') {
                let (key, value) = option.split_once('=').unwrap_or((option, ""));
                if options.insert(key.to_owned(), decode(value)?).is_some() {
                    return Err(invalid("duplicate mount option"));
                }
            }
            Ok(Mount {
                root: PathBuf::from(decode(left[3])?),
                point: PathBuf::from(decode(left[4])?),
                options: left[5].split(',').map(str::to_owned).collect(),
                filesystem: right[0].to_owned(),
                source: decode(right[1])?,
                super_options: options,
            })
        })
        .collect()
}

fn decode(value: &str) -> io::Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let escaped = bytes
                .get(index + 1..index + 4)
                .ok_or_else(|| invalid("truncated mount escape"))?;
            let byte = match escaped {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err(invalid("invalid mount escape")),
            };
            out.push(byte);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| invalid("non-UTF-8 mount path"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mount_binding_and_parser_fail_closed() {
        let generation = Path::new("/store/native/g");
        let lower = "31 1 0:3 / /store/native/g/lower ro,nosuid,nodev - fuse sprig-cas rw,user_id=0,group_id=0\n";
        let overlay = "32 1 0:4 / /store/native/g/tree rw,nosuid,nodev - overlay overlay rw,lowerdir=/store/native/g/lower,upperdir=/store/native/g/upper,workdir=/store/native/g/work,redirect_dir=on\n";
        assert_eq!(
            validate_mounts(generation, &[]).unwrap(),
            MountState::Unmounted
        );
        assert_eq!(
            validate_mounts(generation, &parse_mountinfo(lower).unwrap()).unwrap(),
            MountState::Partial
        );
        let joined = format!("{lower}{overlay}");
        assert_eq!(
            validate_mounts(generation, &parse_mountinfo(&joined).unwrap()).unwrap(),
            MountState::Mounted
        );
        for invalid in [
            joined.replace("user_id=0", "user_id=1"),
            joined.replace("redirect_dir=on", "metacopy=on"),
            joined.replace("g/upper", "other/upper"),
            format!("{joined}{lower}"),
            joined.replace(" - fuse", " - ext4"),
        ] {
            assert!(validate_mounts(generation, &parse_mountinfo(&invalid).unwrap()).is_err());
        }
        assert_eq!(decode("space\\040name").unwrap(), "space name");
        assert!(decode("bad\\000").is_err());
        assert!(parse_mountinfo("malformed").is_err());
    }
}
