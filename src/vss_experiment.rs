//! Opt-in, synthetic-fixture-only ingestion from an immutable VSS snapshot.
//!
//! The trusted requester must retain its VSS lease until this call returns.
//! A device-looking pathname and a marker are not proof of a live VSS lease,
//! human authorization, or application consistency. This module neither creates
//! nor deletes snapshots, enables privileges, or opens the source for writing.
//! It intentionally does not relax the ordinary platform path restrictions.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;

use serde::Deserialize;

use crate::content::{self, Entry, EntryKind, Manifest};

const PREFIX: &str = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy";
const SUFFIX: &str = r"\fixture";
const OUTER_MARKER: &str = ".sprig-vss-synthetic-volume";
const FILES: [&str; 6] = [
    ".sprig-vss-fixture.json",
    "sealed.bin",
    "mapped.bin",
    "written.bin",
    "pre-token.bin",
    "post-token.bin",
];
const PAYLOAD_BYTES: u64 = 8 * 1024 * 1024;
const TOKEN_LIMIT: u64 = 4096;
const JSON_LIMIT: u64 = 16 * 1024;
const GENERIC_READ: u32 = 0x8000_0000;
const SHARE_READ: u32 = 1;
const OPEN_EXISTING: u32 = 3;
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
const ATTRIBUTE_READONLY: u32 = 1;
const ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const EPOCH_TICKS: i128 = 116_444_736_000_000_000;
const INVALID_HANDLE: *mut c_void = -1isize as *mut c_void;

/// Imports only the approved six-file synthetic fixture into a CAS-backed
/// writable workspace. The caller owns the immutable snapshot's lifetime.
/// Object durability and workspace publication remain the Store's transaction.
pub fn import(
    store: &crate::Store,
    root: &Path,
    name: &str,
    serial: u32,
    token: &str,
) -> crate::Result<crate::WorkspaceResult> {
    store.import_streamed_writable(name, |batch| capture(root, serial, token, batch))
}

pub(crate) fn capture(
    root: &Path,
    serial: u32,
    token: &str,
    batch: &mut content::ObjectBatch,
) -> crate::Result<Manifest> {
    let root = root
        .as_os_str()
        .to_str()
        .ok_or_else(|| invalid("snapshot fixture root must be ASCII"))?;
    let device = validate_root(root)?;
    validate_token(token)?;

    // Open and retain each ancestor before opening a child. The caller's VSS
    // lease is what makes these no-follow checks stable, not sharing flags.
    let volume = Node::open(format!("{device}\\"), true, serial)?;
    let outer = Node::open(format!("{device}\\{OUTER_MARKER}"), false, serial)?;
    same_volume(&volume, &outer)?;
    let expected_marker = format!("sprig-vss-synthetic-only-v1\n{token}\n");
    if outer.before.size != expected_marker.len() as u64
        || read_bounded(&outer, expected_marker.len() as u64)? != expected_marker.as_bytes()
    {
        return Err(invalid("synthetic volume marker does not match the expected token").into());
    }

    let fixture = Node::open(root.to_owned(), true, serial)?;
    same_volume(&volume, &fixture)?;
    reject_named_streams(&fixture.path, true)?;
    validate_names(root)?;
    let mut files = Vec::with_capacity(FILES.len());
    for name in FILES {
        let node = Node::open(format!("{root}\\{name}"), false, serial)?;
        same_volume(&volume, &node)?;
        validate_size(name, node.before.size)?;
        reject_named_streams(&node.path, false)?;
        files.push(node);
    }
    reject_named_streams(&outer.path, false)?;

    // Validate bounded metadata before storing any payload. Never interpret a
    // path, size, or filename from this JSON as authority to read other data.
    let marker = read_bounded(&files[0], JSON_LIMIT)?;
    validate_fixture_marker(&marker)?;
    let mut manifest = Manifest::new();
    manifest.insert(
        String::new(),
        entry(root, token, "", &fixture.before, EntryKind::Directory),
    );
    for (name, node) in FILES.into_iter().zip(&files) {
        let kind = if name == FILES[0] {
            batch.put_reader(&mut Cursor::new(&marker))?
        } else {
            let mut source = &node.file;
            let mut exact = ExactReader::new(&mut source, node.before.size);
            let kind = batch.put_reader(&mut exact)?;
            exact.require_finished()?;
            kind
        };
        if !matches!(&kind, EntryKind::File { size, .. } if *size == node.before.size) {
            return Err(invalid("captured size differs from the snapshot handle").into());
        }
        node.recheck(serial)?;
        manifest.insert(
            name.to_owned(),
            entry(root, token, name, &node.before, kind),
        );
    }

    // Recheck all retained handles after the entire capture, including files
    // read early. The namespace/stream queries are safe only within the same
    // immutable snapshot whose lifetime the requester continues to own.
    validate_names(root)?;
    for node in files.iter().chain([&outer, &fixture, &volume]) {
        node.recheck(serial)?;
    }
    reject_named_streams(&fixture.path, true)?;
    reject_named_streams(&outer.path, false)?;
    for node in &files {
        reject_named_streams(&node.path, false)?;
    }
    content::validate_manifest(&manifest)?;
    // The caller must finish this batch before publishing any manifest.
    Ok(manifest)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn validate_root(root: &str) -> io::Result<&str> {
    let number = root
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .ok_or_else(|| invalid("expected the exact VSS device fixture path"))?;
    // Canonical positive decimal only: no case aliases, slash normalization,
    // streams, UNC paths, dot components, extra descendants, or trailing slash.
    if number.is_empty()
        || number.starts_with('0')
        || number.len() > 10
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || number.parse::<u32>().is_err()
    {
        return Err(invalid("invalid VSS shadow-copy device number"));
    }
    Ok(&root[..root.len() - SUFFIX.len()])
}

fn validate_token(token: &str) -> io::Result<()> {
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "synthetic marker token must be exactly 32 hexadecimal digits",
        ));
    }
    Ok(())
}

fn validate_size(name: &str, size: u64) -> io::Result<()> {
    let accepted = match name {
        "sealed.bin" | "mapped.bin" | "written.bin" => size == PAYLOAD_BYTES,
        "pre-token.bin" | "post-token.bin" => size <= TOKEN_LIMIT,
        ".sprig-vss-fixture.json" => size <= JSON_LIMIT,
        _ => false,
    };
    if !accepted {
        return Err(invalid(format!(
            "synthetic fixture size outside fixed bound: {name}"
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureMarker {
    schema_version: u32,
    kind: String,
    fixture_id: String,
    payload_bytes: u64,
    files: Vec<String>,
}

fn validate_fixture_marker(bytes: &[u8]) -> crate::Result<()> {
    if bytes.len() as u64 > JSON_LIMIT {
        return Err(invalid("fixture JSON exceeds its bound").into());
    }
    let marker: FixtureMarker = serde_json::from_slice(bytes)?;
    let canonical_uuid = marker.fixture_id.len() == 36
        && marker.fixture_id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        });
    if marker.schema_version != 1
        || marker.kind != "sprig-vss-synthetic-fixture-v1"
        || !canonical_uuid
        || marker.payload_bytes != PAYLOAD_BYTES
        || marker.files.len() != FILES.len()
        || !marker
            .files
            .iter()
            .zip(FILES)
            .all(|(left, right)| left == right)
    {
        return Err(invalid("fixture JSON differs from the fixed synthetic contract").into());
    }
    Ok(())
}

fn entry(root: &str, token: &str, path: &str, metadata: &Snapshot, kind: EntryKind) -> Entry {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sprig-vss-fixture-entry-v1\0");
    for part in [root, token, path] {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    Entry {
        id: hash.finalize().to_hex().to_string(),
        link_group: None,
        // Match Sprig's canonical writable Windows metadata, not ACLs. A VSS
        // volume's read-only state does not turn file mode into a Unix policy.
        mode: if metadata.directory { 0o755 } else { 0o644 },
        modified_ns: (i128::from(metadata.write) - EPOCH_TICKS) * 100,
        kind,
    }
}

/// Enforces advertised EOF even if the consumer uses short reads. `Take` alone
/// would conceal extra bytes and ordinary EOF would conceal a truncated file.
struct ExactReader<'a, R> {
    inner: &'a mut R,
    remaining: u64,
    checked_eof: bool,
}

impl<'a, R: Read> ExactReader<'a, R> {
    fn new(inner: &'a mut R, size: u64) -> Self {
        Self {
            inner,
            remaining: size,
            checked_eof: false,
        }
    }

    fn require_finished(&self) -> io::Result<()> {
        if self.remaining != 0 || !self.checked_eof {
            return Err(invalid("snapshot reader did not verify exact EOF"));
        }
        Ok(())
    }
}

impl<R: Read> Read for ExactReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() || self.checked_eof {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut extra = [0u8; 1];
            return match self.inner.read(&mut extra) {
                Ok(0) => {
                    self.checked_eof = true;
                    Ok(0)
                }
                Ok(_) => Err(invalid(
                    "snapshot contains bytes beyond its advertised size",
                )),
                Err(error) => Err(error),
            };
        }
        let count = self.remaining.min(bytes.len() as u64) as usize;
        match self.inner.read(&mut bytes[..count]) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "snapshot ended before its advertised size",
            )),
            Ok(read) => {
                self.remaining -= read as u64;
                Ok(read)
            }
            Err(error) => Err(error),
        }
    }
}

fn read_bounded(node: &Node, maximum: u64) -> io::Result<Vec<u8>> {
    if node.before.size > maximum {
        return Err(invalid("snapshot metadata read exceeds its fixed bound"));
    }
    let mut source = &node.file;
    let mut exact = ExactReader::new(&mut source, node.before.size);
    let mut bytes = Vec::with_capacity(node.before.size as usize);
    exact.read_to_end(&mut bytes)?;
    exact.require_finished()?;
    Ok(bytes)
}

struct Node {
    file: File,
    path: String,
    before: Snapshot,
}

impl Node {
    fn open(path: String, directory: bool, serial: u32) -> io::Result<Self> {
        let wide = wide(&path)?;
        // SAFETY: a bounded, validated snapshot path is NUL terminated and live;
        // null security/template pointers are permitted. OPEN_EXISTING and only
        // GENERIC_READ prohibit creating or writing source files. No handle is
        // inherited. OPEN_REPARSE_POINT prevents following the final component.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                OPEN_REPARSE_POINT | BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful CreateFileW returned a uniquely owned live handle.
        let file = unsafe { File::from_raw_handle(raw) };
        let before = snapshot(&file, serial)?;
        if before.directory != directory || (!directory && before.links != 1) {
            return Err(invalid(
                "snapshot node has the wrong type or multiple hard links",
            ));
        }
        if !directory && before.attributes & ATTRIBUTE_READONLY != 0 {
            return Err(invalid(
                "read-only file attributes are outside the writable fixture contract",
            ));
        }
        Ok(Self { file, path, before })
    }

    fn recheck(&self, serial: u32) -> io::Result<()> {
        if snapshot(&self.file, serial)? != self.before {
            return Err(invalid(format!(
                "snapshot handle changed during capture: {}",
                self.path
            )));
        }
        Ok(())
    }
}

fn same_volume(root: &Node, child: &Node) -> io::Result<()> {
    if root.before.volume != child.before.volume {
        return Err(invalid("snapshot node is on a different physical volume"));
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Snapshot {
    volume: u64,
    identifier: [u8; 16],
    size: u64,
    links: u32,
    attributes: u32,
    directory: bool,
    creation: i64,
    write: i64,
    change: i64,
}

fn snapshot(file: &File, expected_serial: u32) -> io::Result<Snapshot> {
    let mut identity = FileIdInfo::default();
    let mut basic = BasicInfo::default();
    let mut standard = StandardInfo::default();
    let mut serial = 0;
    let mut filesystem = [0u16; 32];
    // SAFETY: File owns the handle for every call; all output objects have the
    // documented repr(C) layout, correct size, and remain writable and live.
    if unsafe { GetFileType(file.as_raw_handle()) } != 1 {
        return Err(invalid("snapshot source must be a disk filesystem node"));
    }
    if unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if serial != expected_serial || terminated(&filesystem)? != [78, 84, 70, 83] {
        return Err(invalid(
            "snapshot must be NTFS with the explicitly expected volume serial",
        ));
    }
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            (&mut identity as *mut FileIdInfo).cast(),
            std::mem::size_of::<FileIdInfo>() as u32,
        )
    } == 0
        || unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                0,
                (&mut basic as *mut BasicInfo).cast(),
                std::mem::size_of::<BasicInfo>() as u32,
            )
        } == 0
        || unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                1,
                (&mut standard as *mut StandardInfo).cast(),
                std::mem::size_of::<StandardInfo>() as u32,
            )
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if basic.attributes & ATTRIBUTE_REPARSE_POINT != 0
        || standard.delete_pending != 0
        || standard.directory > 1
        || standard.end_of_file < 0
        || (basic.attributes & ATTRIBUTE_DIRECTORY != 0) != (standard.directory != 0)
    {
        return Err(invalid(
            "snapshot node is a reparse point, deleted, or has invalid metadata",
        ));
    }
    Ok(Snapshot {
        volume: identity.volume,
        identifier: identity.identifier,
        size: standard.end_of_file as u64,
        links: standard.number_of_links,
        attributes: basic.attributes,
        directory: standard.directory != 0,
        creation: basic.creation,
        write: basic.write,
        change: basic.change,
    })
}

fn wide(path: &str) -> io::Result<Vec<u16>> {
    if path.contains('\0') {
        return Err(invalid("NUL in snapshot path"));
    }
    Ok(path.encode_utf16().chain(Some(0)).collect())
}

fn terminated(buffer: &[u16]) -> io::Result<&[u16]> {
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| invalid("unterminated Windows enumeration name"))?;
    Ok(&buffer[..end])
}

struct Search(*mut c_void);
impl Drop for Search {
    fn drop(&mut self) {
        // SAFETY: uniquely owned successful FindFirst* enumeration handle.
        unsafe {
            FindClose(self.0);
        }
    }
}

fn validate_names(root: &str) -> io::Result<()> {
    let pattern = wide(&format!("{root}\\*"))?;
    let mut data = FindData::default();
    // SAFETY: immutable checked directory plus a fixed wildcard; the live,
    // writable buffer has the WIN32_FIND_DATAW ABI. No child is opened here.
    let raw = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
    if raw == INVALID_HANDLE {
        return Err(io::Error::last_os_error());
    }
    let search = Search(raw);
    let mut seen = [false; FILES.len()];
    loop {
        let name = String::from_utf16(terminated(&data.name)?)
            .map_err(|_| invalid("non-Unicode fixture filename"))?;
        if name != "." && name != ".." {
            let index = FILES
                .iter()
                .position(|expected| *expected == name)
                .ok_or_else(|| invalid("unexpected name in fixed snapshot fixture"))?;
            if seen[index] || data.attributes & (ATTRIBUTE_DIRECTORY | ATTRIBUTE_REPARSE_POINT) != 0
            {
                return Err(invalid("duplicate, directory, or reparse entry in fixture"));
            }
            seen[index] = true;
        }
        // SAFETY: the live enumeration owns a correctly sized writable buffer.
        if unsafe { FindNextFileW(search.0, &mut data) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(18) {
                return Err(error);
            }
            break;
        }
    }
    if seen.iter().any(|found| !found) {
        return Err(invalid("fixed snapshot fixture is missing required files"));
    }
    Ok(())
}

fn reject_named_streams(path: &str, directory: bool) -> io::Result<()> {
    let path = wide(path)?;
    let mut data = StreamInfo {
        size: 0,
        name: [0; 296],
    };
    // SAFETY: this path belongs only to the already checked immutable snapshot;
    // the requester retains its namespace lease throughout path enumeration.
    let raw = unsafe { FindFirstStreamW(path.as_ptr(), 0, &mut data, 0) };
    if raw == INVALID_HANDLE {
        let error = io::Error::last_os_error();
        return if directory && error.raw_os_error() == Some(38) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let search = Search(raw);
    let mut seen_default = false;
    loop {
        if directory || seen_default || terminated(&data.name)? != [58, 58, 36, 68, 65, 84, 65] {
            return Err(invalid(
                "named or duplicate Windows data streams are outside the fixture contract",
            ));
        }
        seen_default = true;
        // SAFETY: the owned live enumeration and correctly sized output persist.
        if unsafe { FindNextStreamW(search.0, &mut data) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(38) {
                Ok(())
            } else {
                Err(error)
            };
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct FileIdInfo {
    volume: u64,
    identifier: [u8; 16],
}
#[repr(C)]
#[derive(Default)]
struct BasicInfo {
    creation: i64,
    access: i64,
    write: i64,
    change: i64,
    attributes: u32,
}
#[repr(C)]
#[derive(Default)]
struct StandardInfo {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    delete_pending: u8,
    directory: u8,
}
#[repr(C)]
struct StreamInfo {
    size: i64,
    name: [u16; 296],
}
#[repr(C)]
struct FindData {
    attributes: u32,
    creation: [u32; 2],
    access: [u32; 2],
    write: [u32; 2],
    size_high: u32,
    size_low: u32,
    reserved0: u32,
    reserved1: u32,
    name: [u16; 260],
    alternate: [u16; 14],
}
impl Default for FindData {
    fn default() -> Self {
        Self {
            attributes: 0,
            creation: [0; 2],
            access: [0; 2],
            write: [0; 2],
            size_high: 0,
            size_low: 0,
            reserved0: 0,
            reserved1: 0,
            name: [0; 260],
            alternate: [0; 14],
        }
    }
}
const _: () = {
    assert!(std::mem::size_of::<FileIdInfo>() == 24);
    assert!(std::mem::offset_of!(FileIdInfo, identifier) == 8);
    assert!(std::mem::size_of::<BasicInfo>() == 40);
    assert!(std::mem::offset_of!(BasicInfo, attributes) == 32);
    assert!(std::mem::size_of::<StandardInfo>() == 24);
    assert!(std::mem::offset_of!(StandardInfo, directory) == 21);
    assert!(std::mem::size_of::<StreamInfo>() == 600);
    assert!(std::mem::offset_of!(StreamInfo, name) == 8);
    assert!(std::mem::size_of::<FindData>() == 592);
    assert!(std::mem::offset_of!(FindData, name) == 44);
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn GetFileType(handle: *mut c_void) -> u32;
    fn GetFileInformationByHandleEx(
        handle: *mut c_void,
        class: i32,
        info: *mut c_void,
        size: u32,
    ) -> i32;
    fn GetVolumeInformationByHandleW(
        handle: *mut c_void,
        name: *mut u16,
        name_size: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        filesystem: *mut u16,
        filesystem_size: u32,
    ) -> i32;
    fn FindFirstFileW(path: *const u16, info: *mut FindData) -> *mut c_void;
    fn FindNextFileW(handle: *mut c_void, info: *mut FindData) -> i32;
    fn FindFirstStreamW(
        path: *const u16,
        level: i32,
        info: *mut StreamInfo,
        flags: u32,
    ) -> *mut c_void;
    fn FindNextStreamW(handle: *mut c_void, info: *mut StreamInfo) -> i32;
    fn FindClose(handle: *mut c_void) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_token_are_narrowly_scoped() {
        assert!(validate_root(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\fixture").is_ok());
        for root in [
            r"C:\fixture",
            r"\\server\share\fixture",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy0\fixture",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy01\fixture",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\fixture\",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\fixture\child",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\..\fixture",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\fixture:ads",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4294967296\fixture",
            r"\\?\globalroot\Device\HarddiskVolumeShadowCopy1\fixture",
        ] {
            assert!(validate_root(root).is_err(), "{root}");
        }
        assert!(validate_token("0123456789abcdef0123456789ABCDEF").is_ok());
        for token in [
            "",
            "0123456789abcdef0123456789abcdef\n",
            "g123456789abcdef0123456789abcdef",
        ] {
            assert!(validate_token(token).is_err());
        }
    }

    struct Fragmented {
        bytes: Cursor<Vec<u8>>,
        interrupt: bool,
    }
    impl Read for Fragmented {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.interrupt {
                self.interrupt = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.interrupt = true;
            let length = buffer.len().min(2);
            self.bytes.read(&mut buffer[..length])
        }
    }

    #[test]
    fn exact_reader_handles_short_reads_interrupts_and_empty_files() {
        for bytes in [Vec::new(), b"bounded bytes".to_vec()] {
            let mut source = Fragmented {
                bytes: Cursor::new(bytes.clone()),
                interrupt: true,
            };
            let mut exact = ExactReader::new(&mut source, bytes.len() as u64);
            assert!(exact.require_finished().is_err());
            assert_eq!(exact.read(&mut []).unwrap(), 0);
            let mut result = Vec::new();
            exact.read_to_end(&mut result).unwrap();
            exact.require_finished().unwrap();
            assert_eq!(result, bytes);
        }
    }

    #[test]
    fn exact_reader_rejects_both_eof_mismatches() {
        for (bytes, expected, kind) in [
            (b"abc".as_slice(), 4, io::ErrorKind::UnexpectedEof),
            (b"abc".as_slice(), 2, io::ErrorKind::InvalidData),
            (b"a".as_slice(), 0, io::ErrorKind::InvalidData),
        ] {
            let mut source = Cursor::new(bytes);
            let mut exact = ExactReader::new(&mut source, expected);
            assert_eq!(exact.read_to_end(&mut Vec::new()).unwrap_err().kind(), kind);
            assert!(exact.require_finished().is_err());
        }
    }

    #[test]
    fn manifest_metadata_is_canonical_and_ids_are_scoped() {
        let metadata = Snapshot {
            volume: 42,
            identifier: [1; 16],
            size: 0,
            links: 1,
            attributes: 0,
            directory: false,
            creation: 0,
            write: EPOCH_TICKS as i64 + 17,
            change: 0,
        };
        let file = entry(
            "root",
            "token",
            "sealed.bin",
            &metadata,
            EntryKind::File {
                size: 0,
                chunks: Vec::new(),
            },
        );
        let again = entry(
            "root",
            "token",
            "sealed.bin",
            &metadata,
            EntryKind::File {
                size: 0,
                chunks: Vec::new(),
            },
        );
        assert_eq!(file, again);
        assert_eq!(file.mode, 0o644);
        assert_eq!(file.modified_ns, 1700);
        assert_eq!(file.link_group, None);
        let mut directory = metadata;
        directory.directory = true;
        let root = entry("root", "token", "", &directory, EntryKind::Directory);
        assert_eq!(root.mode, 0o755);
        assert_ne!(root.id, file.id);
        assert_ne!(
            entry(
                "other-root",
                "token",
                "sealed.bin",
                &directory,
                EntryKind::Directory
            )
            .id,
            file.id
        );
        assert_ne!(
            entry(
                "root",
                "other-token",
                "sealed.bin",
                &directory,
                EntryKind::Directory
            )
            .id,
            file.id
        );
    }

    #[test]
    fn sizes_and_json_cannot_expand_the_fixture() {
        assert!(validate_size("sealed.bin", PAYLOAD_BYTES).is_ok());
        assert!(validate_size("sealed.bin", PAYLOAD_BYTES - 1).is_err());
        assert!(validate_size("pre-token.bin", TOKEN_LIMIT + 1).is_err());
        assert!(validate_size(".sprig-vss-fixture.json", JSON_LIMIT + 1).is_err());
        assert!(validate_size("other.bin", 0).is_err());
        let marker = serde_json::json!({
            "schema_version": 1, "kind": "sprig-vss-synthetic-fixture-v1",
            "fixture_id": "12345678-1234-1234-1234-123456789abc",
            "payload_bytes": PAYLOAD_BYTES, "files": FILES,
        });
        assert!(validate_fixture_marker(&serde_json::to_vec(&marker).unwrap()).is_ok());
        let mut changed = marker.clone();
        changed["files"][0] = serde_json::json!("../outside");
        assert!(validate_fixture_marker(&serde_json::to_vec(&changed).unwrap()).is_err());
        let mut changed = marker;
        changed["payload_bytes"] = serde_json::json!(PAYLOAD_BYTES + 1);
        assert!(validate_fixture_marker(&serde_json::to_vec(&changed).unwrap()).is_err());
    }
}
