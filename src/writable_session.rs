//! Fail-closed certification for cached writable mount sessions.
//!
//! A session marker survives daemon crashes. Only the lifecycle controller,
//! after normal unmount and lease release, may finish a certified stop. The
//! daemon acknowledges only after zero open handles, no sticky errors, and a
//! successful core sync; destruction alone is never a clean-stop certificate.
//! Callers serialize lifecycle changes and keep generation directories trusted
//! and quiescent. Platform helpers retain their Windows durability limitations.

use std::fs;
use std::io;
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
use std::io::Read;
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
use std::io::Write;
use std::path::Path;
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
use serde::{Deserialize, Serialize};

use crate::platform;

const ACTIVE: &str = "writable-session.json";
const STOP: &str = "writable-stop.json";
const CLEAN: &str = "writable-clean.json";
const STOP_TMP: &str = "writable-stop.json.tmp";
const CLEAN_TMP: &str = "writable-clean.json.tmp";
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
const MAX_RECORD_BYTES: u64 = 4096;
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
static NEXT_NONCE: AtomicU64 = AtomicU64::new(0);

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionRecord {
    pub version: u32,
    pub generation: String,
    pub revision: String,
    pub nonce: String,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn generation_name(generation: &Path) -> io::Result<&str> {
    let metadata = platform::symlink_metadata(generation)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid(
            "writable session requires a real generation directory",
        ));
    }
    let name = generation
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("invalid writable session generation"))?;
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid("invalid writable session generation"));
    }
    Ok(name)
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
fn validate(generation: &Path, record: &SessionRecord) -> io::Result<()> {
    if record.version != 1
        || record.generation != generation_name(generation)?
        || record.revision.len() != 64
        || !record
            .revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || record.nonce.is_empty()
        || record.nonce.len() > 128
        || !record
            .nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(invalid(
            "invalid writable session record or generation binding",
        ));
    }
    Ok(())
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
fn read_record(generation: &Path, name: &str) -> io::Result<SessionRecord> {
    let file = platform::open_regular(&generation.join(name))?;
    if platform::file_metadata(&file)?.len() > MAX_RECORD_BYTES {
        return Err(invalid("writable session record exceeds size limit"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("writable session record exceeds size limit"));
    }
    let record =
        serde_json::from_slice(&bytes).map_err(|_| invalid("corrupt writable session record"))?;
    validate(generation, &record)?;
    Ok(record)
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
fn require_record(generation: &Path, name: &str, expected: &SessionRecord) -> io::Result<()> {
    validate(generation, expected)?;
    if read_record(generation, name)? != *expected {
        return Err(invalid("stale or mismatched writable session certificate"));
    }
    Ok(())
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
fn absent(generation: &Path, name: &str) -> io::Result<bool> {
    match platform::symlink_metadata(generation.join(name)) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Existence only: corrupt or partial markers still force read-only recovery.
/// Never use marker contents to select a base or a path. Explicit recovery uses
/// the independently verified Store pin and journal and preserves every marker.
pub(crate) fn has_session(generation: &Path) -> io::Result<bool> {
    generation_name(generation)?;
    for name in [ACTIVE, STOP, CLEAN, STOP_TMP, CLEAN_TMP] {
        // lstat does not follow a leaf symlink/junction. Any node counts as an
        // abandoned session; normal certification uses strict nofollow reads.
        match fs::symlink_metadata(generation.join(name)) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// Also rejects stray certificates and interrupted certificate publications.
/// Recovery must preserve the old generation, not erase these markers.
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
pub(crate) fn ensure_clean(generation: &Path) -> io::Result<()> {
    generation_name(generation)?;
    for name in [ACTIVE, STOP, CLEAN, STOP_TMP, CLEAN_TMP] {
        if !absent(generation, name)? {
            return Err(invalid(
                "unclean writable session; recover into a new generation",
            ));
        }
    }
    Ok(())
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
fn write_new(generation: &Path, name: &str, record: &SessionRecord) -> io::Result<()> {
    validate(generation, record)?;
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("writable session record exceeds size limit"));
    }
    let mut file = platform::create_new(&generation.join(name), 0o600)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    platform::sync_directory(generation)
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
pub(crate) fn begin(generation: &Path, revision: &str) -> io::Result<SessionRecord> {
    ensure_clean(generation)?;
    // This is a uniqueness tag, not an authentication secret. create_new and
    // generation isolation are the authority, not unpredictability of the tag.
    let record = SessionRecord {
        version: 1,
        generation: generation_name(generation)?.to_owned(),
        revision: revision.to_owned(),
        nonce: format!(
            "{:x}-{:x}-{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos(),
            std::process::id(),
            NEXT_NONCE.fetch_add(1, Ordering::Relaxed)
        ),
    };
    write_new(generation, ACTIVE, &record)?;
    Ok(record)
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
fn publish(
    generation: &Path,
    name: &str,
    temporary: &str,
    record: &SessionRecord,
) -> io::Result<()> {
    // Never silently overwrite corrupt, unsafe or stale certificates. An exact
    // existing certificate is an idempotent retry, but still needs durability.
    if !absent(generation, name)? {
        require_record(generation, name, record)?;
        platform::sync_file(&generation.join(name))?;
        return platform::sync_directory(generation);
    }
    write_new(generation, temporary, record)?;
    platform::rename(&generation.join(temporary), &generation.join(name))?;
    platform::sync_directory(generation)
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
pub(crate) fn request_stop(generation: &Path) -> io::Result<SessionRecord> {
    let record = read_record(generation, ACTIVE)?;
    publish(generation, STOP, STOP_TMP, &record)?;
    Ok(record)
}

/// Missing stop is ordinary while running; a missing active marker is not.
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
pub(crate) fn read_stop(generation: &Path, expected: &SessionRecord) -> io::Result<bool> {
    require_record(generation, ACTIVE, expected)?;
    if absent(generation, STOP)? {
        return Ok(false);
    }
    require_record(generation, STOP, expected)?;
    Ok(true)
}

#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
pub(crate) fn acknowledge(generation: &Path, expected: &SessionRecord) -> io::Result<()> {
    if !read_stop(generation, expected)? {
        return Err(invalid(
            "cannot certify a writable session without a stop request",
        ));
    }
    publish(generation, CLEAN, CLEAN_TMP, expected)
}

/// Call only after normal unmount and daemon lease release. Never call from
/// Filesystem::destroy. Every missing marker, including on retry, is an error.
#[cfg(any(
    test,
    all(target_os = "linux", feature = "projection"),
    all(windows, feature = "windows-mount")
))]
pub(crate) fn finish_stop(generation: &Path, expected: &SessionRecord) -> io::Result<()> {
    for name in [ACTIVE, STOP, CLEAN] {
        require_record(generation, name, expected)?;
    }
    for name in [STOP_TMP, CLEAN_TMP] {
        if !absent(generation, name)? {
            return Err(invalid(
                "interrupted writable session certificate publication",
            ));
        }
    }
    fs::remove_file(generation.join(CLEAN))?;
    fs::remove_file(generation.join(STOP))?;
    platform::sync_directory(generation)?;
    // Active is removed last, so an interrupted cleanup cannot advertise a
    // clean generation before removal of the associated certificates is durable.
    fs::remove_file(generation.join(ACTIVE))?;
    platform::sync_directory(generation)
}

/// Only the Windows controller uses this to bind its ready/busy handshakes.
#[cfg(all(windows, feature = "windows-mount"))]
pub(crate) fn active_record(generation: &Path) -> io::Result<SessionRecord> {
    read_record(generation, ACTIVE)
}

/// A busy stop request may be cancelled, but never a certified or uncertain stop.
#[cfg(all(windows, feature = "windows-mount"))]
pub(crate) fn cancel_stop(generation: &Path, expected: &SessionRecord) -> io::Result<()> {
    require_record(generation, ACTIVE, expected)?;
    require_record(generation, STOP, expected)?;
    for name in [CLEAN, CLEAN_TMP, STOP_TMP] {
        if !absent(generation, name)? {
            return Err(invalid("cannot cancel a certified or interrupted stop"));
        }
    }
    fs::remove_file(generation.join(STOP))?;
    platform::sync_directory(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_phases_reject_corruption_stale_certificates_and_remaining_markers() {
        let root = std::env::temp_dir().join(format!(
            "sprig-session-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let revision = "a".repeat(64);
        ensure_clean(&root).unwrap();
        assert!(!has_session(&root).unwrap());
        let record = begin(&root, &revision).unwrap();
        assert!(ensure_clean(&root).is_err());
        assert!(has_session(&root).unwrap());
        assert!(begin(&root, &revision).is_err());
        assert!(!read_stop(&root, &record).unwrap());
        assert!(acknowledge(&root, &record).is_err());
        assert!(finish_stop(&root, &record).is_err());
        assert_eq!(request_stop(&root).unwrap(), record);
        assert_eq!(request_stop(&root).unwrap(), record);
        let mut stale = record.clone();
        stale.nonce.push('a');
        assert!(acknowledge(&root, &stale).is_err());
        write_new(&root, CLEAN, &stale).unwrap();
        assert!(finish_stop(&root, &record).is_err());
        assert!(root.join(ACTIVE).is_file());
        assert!(has_session(&root).unwrap());
        fs::remove_file(root.join(CLEAN)).unwrap();
        acknowledge(&root, &record).unwrap();
        acknowledge(&root, &record).unwrap();
        fs::write(root.join(CLEAN), b"{}").unwrap();
        assert!(finish_stop(&root, &record).is_err());
        fs::write(root.join(CLEAN), vec![b' '; MAX_RECORD_BYTES as usize + 1]).unwrap();
        assert!(finish_stop(&root, &record).is_err());
        fs::remove_file(root.join(CLEAN)).unwrap();
        acknowledge(&root, &record).unwrap();
        fs::write(root.join(STOP_TMP), b"interrupted").unwrap();
        assert!(finish_stop(&root, &record).is_err());
        fs::remove_file(root.join(STOP_TMP)).unwrap();
        finish_stop(&root, &record).unwrap();
        ensure_clean(&root).unwrap();
        assert!(finish_stop(&root, &record).is_err());
        assert!(request_stop(&root).is_err());
        fs::write(root.join(ACTIVE), b"{partial").unwrap();
        assert!(has_session(&root).unwrap());
        assert!(request_stop(&root).is_err());
        fs::remove_file(root.join(ACTIVE)).unwrap();
        fs::create_dir(root.join(ACTIVE)).unwrap();
        assert!(ensure_clean(&root).is_err());
        assert!(request_stop(&root).is_err());
        fs::remove_dir(root.join(ACTIVE)).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("missing-target", root.join(ACTIVE)).unwrap();
            assert!(has_session(&root).unwrap());
            assert!(ensure_clean(&root).is_err());
            assert!(request_stop(&root).is_err());
            fs::remove_file(root.join(ACTIVE)).unwrap();
        }
        write_new(&root, CLEAN, &record).unwrap();
        assert!(has_session(&root).unwrap());
        assert!(ensure_clean(&root).is_err());
        assert!(begin(&root, &revision).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
