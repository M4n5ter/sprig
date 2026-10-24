//! Opt-in, bounded fixture ingestion from an immutable VSS snapshot.
//!
//! The trusted requester must retain its VSS lease until this call returns.
//! A device-looking pathname and a marker are not proof of a live VSS lease,
//! human authorization, or application consistency. This module neither creates
//! nor deletes snapshots, enables privileges, or opens the source for writing.
//! It intentionally does not relax the ordinary platform path restrictions.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
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
const TREE_BYTES: u64 = 32 * 1024 * 1024;
const TREE_ENTRIES: usize = 4096;
const TREE_DEPTH: usize = 64;
// FILE_GENERIC_READ after Windows has expanded GENERIC_READ. No write, delete,
// owner, DACL, execute/traverse, maximum-allowed, or generic access is accepted.
const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_READ_DATA_ATTRIBUTES: u32 = 0x0081;
const NT_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const NT_SYNCHRONOUS_NONALERT: u32 = 0x0020;
const NT_DIRECTORY_FILE: u32 = 0x0001;
const NT_NON_DIRECTORY_FILE: u32 = 0x0040;
const OBJ_DONT_REPARSE: u32 = 0x1000;
const QUERY_BYTES: usize = 64 * 1024;

/// Imports the complete bounded tree beneath an owned read-only directory
/// handle. The trusted requester must keep its immutable VSS lease alive until
/// this returns. `expected_root` is an exact binding assertion, never authority
/// to open the tree: only the inherited handle authorizes descendant opens.
///
/// The only accepted root is an explicitly expected VSS device's `\\fixture`.
/// The volume serial and outer marker token are checked independently. This
/// neither creates a snapshot nor establishes human authorization or a lease.
/// At most 4096 entries (including root), depth 64, and 32 MiB of logical file
/// contents (counting every hard-link alias) are accepted. All files are read;
/// timestamps, USNs, and prior captures are never used to skip content.
pub fn import_tree(
    store: &crate::Store,
    root: OwnedHandle,
    expected_root: &Path,
    name: &str,
    serial: u32,
    token: &str,
) -> crate::Result<crate::WorkspaceResult> {
    let capability = SnapshotTree::from_handle(root, expected_root, serial, token)?;
    store.import_streamed_writable(name, |batch| capability.capture(batch))
}

// Deliberately private and non-Clone. The handle binds descendant reads, but
// VSS lease lifetime is still enforced by the trusted requester/job protocol,
// not by this Rust type or by the directory handle alone.
struct SnapshotTree {
    root: Node,
    volume: Node,
    marker: Node,
    serial: u32,
    token: String,
}

impl SnapshotTree {
    fn from_handle(
        root: OwnedHandle,
        expected_root: &Path,
        serial: u32,
        token: &str,
    ) -> crate::Result<Self> {
        validate_token(token)?;
        let expected = expected_root
            .to_str()
            .ok_or_else(|| invalid("expected snapshot fixture root must be ASCII"))?;
        validate_root(expected)?;
        let file = File::from(root);
        require_read_only_handle(&file)?;
        let before = snapshot(&file, serial)?;
        if !before.directory {
            return Err(invalid("snapshot capability must own a directory handle").into());
        }
        // Query the device name from the handle, not from a command-line path.
        let path = final_device_path(&file)?;
        let device = validate_root_binding(&path, expected)?.to_owned();
        let root = Node { file, path, before };
        validate_tree_metadata(&root)?;

        // The only absolute open is the already handle-derived device root.
        // Keep this ancestor and the inherited root alive for the entire scan.
        let volume = Node::open(format!("{device}\\"), true, serial)?;
        same_volume(&volume, &root)?;
        let bound_root = Node::open_relative(&volume, "fixture", true, serial)?;
        if bound_root.before != root.before || final_device_path(&bound_root.file)? != root.path {
            return Err(invalid("inherited handle does not bind to the approved fixture").into());
        }
        let marker = Node::open_relative(&volume, OUTER_MARKER, false, serial)?;
        same_volume(&volume, &marker)?;
        validate_tree_metadata(&marker)?;
        let expected_marker = format!("sprig-vss-synthetic-only-v1\n{token}\n");
        if marker.before.links != 1
            || marker.before.size != expected_marker.len() as u64
            || read_bounded(&marker, expected_marker.len() as u64)? != expected_marker.as_bytes()
        {
            return Err(
                invalid("synthetic volume marker does not match the expected token").into(),
            );
        }
        Ok(Self {
            root,
            volume,
            marker,
            serial,
            token: token.to_owned(),
        })
    }

    fn capture(self, batch: &mut content::ObjectBatch) -> crate::Result<Manifest> {
        let Self {
            root,
            volume,
            marker,
            serial,
            token,
        } = self;
        let root_path = root.path.clone();
        let mut nodes = vec![TreeNode {
            node: root,
            relative: String::new(),
            parent: None,
            depth: 0,
            children: None,
        }];
        let mut budget = TreeBudget::default();
        budget.add(0, true, 0)?;
        // First inspect the entire tree and all metadata before writing objects.
        // Every opened directory and leaf stays owned until capture is complete.
        let mut index = 0;
        while index < nodes.len() {
            if nodes[index].node.before.directory {
                let children = directory_children(&nodes[index].node.file)?;
                for child in &children {
                    let depth = nodes[index].depth + 1;
                    if depth > TREE_DEPTH || nodes.len() >= TREE_ENTRIES {
                        return Err(invalid("snapshot exceeds its depth or entry bound").into());
                    }
                    let node = Node::open_relative(
                        &nodes[index].node,
                        &child.name,
                        child.directory,
                        serial,
                    )?;
                    same_volume(&volume, &node)?;
                    validate_tree_metadata(&node)?;
                    budget.add(depth, node.before.directory, node.before.size)?;
                    let relative = if nodes[index].relative.is_empty() {
                        child.name.clone()
                    } else {
                        format!("{}/{}", nodes[index].relative, child.name)
                    };
                    nodes.push(TreeNode {
                        node,
                        relative,
                        parent: Some(index),
                        depth,
                        children: None,
                    });
                }
                nodes[index].children = Some(children);
            }
            index += 1;
        }
        validate_tree_links(nodes.iter().map(|item| &item.node.before))?;

        let mut manifest = Manifest::new();
        let mut observed = content::Observed::new();
        let mut physical = BTreeMap::<(u64, [u8; 16]), (&Snapshot, EntryKind)>::new();
        for item in &nodes {
            let node = &item.node;
            let kind = if node.before.directory {
                EntryKind::Directory
            } else {
                let mut source = &node.file;
                let mut exact = ExactReader::new(&mut source, node.before.size);
                let kind = batch.put_reader(&mut exact)?;
                exact.require_finished()?;
                if !matches!(&kind, EntryKind::File { size, .. } if *size == node.before.size) {
                    return Err(invalid("captured size differs from snapshot handle").into());
                }
                // Read every alias, rather than silently skipping based on ID.
                // A physical identity must agree on all supported metadata and
                // content before it can become a canonical hard-link group.
                let key = (node.before.volume, node.before.identifier);
                if let Some((prior, prior_kind)) = physical.get(&key) {
                    if *prior != &node.before || prior_kind != &kind {
                        return Err(invalid("hard-link aliases disagree during capture").into());
                    }
                } else {
                    physical.insert(key, (&node.before, kind.clone()));
                }
                kind
            };
            node.recheck(serial)?;
            let value = entry(&root_path, &token, &item.relative, &node.before, kind);
            observed.insert(
                item.relative.clone(),
                content::Observation {
                    id: value.id.clone(),
                    link_group: None,
                    device: node.before.volume,
                    inode: u128::from_le_bytes(node.before.identifier),
                },
            );
            manifest.insert(item.relative.clone(), value);
        }

        // Repeat complete namespace/stream/identity checks, including paths read
        // early. A name rebound to another inode must not pass a retained-handle
        // check: reopen that single name relative to its still-owned parent.
        for item in &nodes {
            item.node.recheck(serial)?;
            validate_tree_metadata(&item.node)?;
            if let Some(children) = &item.children {
                if directory_children(&item.node.file)? != *children {
                    return Err(invalid("snapshot directory changed during capture").into());
                }
            }
            if let Some(parent) = item.parent {
                let name = item.relative.rsplit('/').next().expect("non-root name");
                let current = Node::open_relative(
                    &nodes[parent].node,
                    name,
                    item.node.before.directory,
                    serial,
                )?;
                if current.before != item.node.before {
                    return Err(invalid("snapshot name changed its physical identity").into());
                }
            }
        }
        if final_device_path(&nodes[0].node.file)? != root_path {
            return Err(invalid("snapshot root binding changed during capture").into());
        }
        let bound_root = Node::open_relative(&volume, "fixture", true, serial)?;
        let bound_marker = Node::open_relative(&volume, OUTER_MARKER, false, serial)?;
        if bound_root.before != nodes[0].node.before || bound_marker.before != marker.before {
            return Err(invalid("snapshot fixture or marker binding changed").into());
        }
        volume.recheck(serial)?;
        marker.recheck(serial)?;
        validate_tree_metadata(&marker)?;
        // Use the ordinary canonical path-ID/link-group assignment, including
        // ungrouped singleton files. Store still owns batch finish/publication.
        content::assign_link_groups(&mut manifest, &mut observed, &content::Observed::new())?;
        Ok(manifest)
    }
}

struct TreeNode {
    node: Node,
    relative: String,
    parent: Option<usize>,
    depth: usize,
    children: Option<Vec<ChildName>>,
}

#[derive(Default)]
struct TreeBudget {
    entries: usize,
    bytes: u64,
}

impl TreeBudget {
    fn add(&mut self, depth: usize, directory: bool, size: u64) -> io::Result<()> {
        let entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| invalid("entry count overflow"))?;
        let bytes = self
            .bytes
            .checked_add(if directory { 0 } else { size })
            .ok_or_else(|| invalid("snapshot logical byte count overflow"))?;
        if depth > TREE_DEPTH || entries > TREE_ENTRIES || bytes > TREE_BYTES {
            return Err(invalid(
                "snapshot exceeds 4096 entries, depth 64, or 32 MiB logical content",
            ));
        }
        self.entries = entries;
        self.bytes = bytes;
        Ok(())
    }
}

fn validate_tree_links<'a>(metadata: impl Iterator<Item = &'a Snapshot>) -> io::Result<()> {
    let mut identities = BTreeMap::<(u64, [u8; 16]), (&Snapshot, u32)>::new();
    for value in metadata {
        let key = (value.volume, value.identifier);
        if let Some((prior, count)) = identities.get_mut(&key) {
            if value.directory || *prior != value {
                return Err(invalid(
                    "duplicate directory identity or inconsistent hard-link metadata",
                ));
            }
            *count = count
                .checked_add(1)
                .ok_or_else(|| invalid("hard-link count overflow"))?;
        } else {
            identities.insert(key, (value, 1));
        }
    }
    for (value, count) in identities.values() {
        if !value.directory && value.links != *count {
            return Err(invalid(
                "snapshot hard links escape the approved fixture root",
            ));
        }
    }
    Ok(())
}

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

fn validate_root_binding<'a>(actual: &'a str, expected: &str) -> io::Result<&'a str> {
    validate_root(expected)?;
    let device = validate_root(actual)?;
    if actual != expected {
        return Err(invalid(
            "inherited handle is not the exact approved VSS fixture root",
        ));
    }
    Ok(device)
}

fn validate_granted_access(access: u32) -> io::Result<()> {
    if access & FILE_READ_DATA_ATTRIBUTES != FILE_READ_DATA_ATTRIBUTES
        || access & !FILE_GENERIC_READ != 0
    {
        return Err(invalid(
            "snapshot capability must grant only read access, including list/read-attributes",
        ));
    }
    Ok(())
}

fn nt_result(status: i32) -> io::Result<()> {
    if status < 0 {
        // SAFETY: this routine maps an NTSTATUS value and has no pointer inputs.
        Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ))
    } else if status != 0 {
        // These synchronous opens/queries require STATUS_SUCCESS, not merely
        // NT_SUCCESS. In particular, STATUS_PENDING is not completed output.
        Err(invalid(format!(
            "unexpected non-success NTSTATUS: {status:#010x}"
        )))
    } else {
        Ok(())
    }
}

fn require_read_only_handle(file: &File) -> io::Result<()> {
    let mut basic = ObjectBasicInfo::default();
    let mut returned = 0;
    // SAFETY: File owns a live handle and the output matches the documented
    // PUBLIC_OBJECT_BASIC_INFORMATION layout. Querying access changes nothing.
    nt_result(unsafe {
        NtQueryObject(
            file.as_raw_handle(),
            0,
            (&mut basic as *mut ObjectBasicInfo).cast(),
            std::mem::size_of::<ObjectBasicInfo>() as u32,
            &mut returned,
        )
    })?;
    if returned as usize != std::mem::size_of::<ObjectBasicInfo>() {
        return Err(invalid("unexpected object access-information size"));
    }
    validate_granted_access(basic.granted_access)
}

fn final_device_path(file: &File) -> io::Result<String> {
    // Bounded even if a malicious/unsupported driver advertises a huge name.
    let mut buffer = vec![0u16; 32768];
    // SAFETY: owned live file and writable UTF-16 output. VOLUME_NAME_NT (2)
    // returns the final device path without needing a drive-letter mapping.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            2,
        )
    } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(invalid("snapshot handle path exceeds its bound"));
    }
    let nt = String::from_utf16(&buffer[..length])
        .map_err(|_| invalid("snapshot handle has a non-Unicode path"))?;
    if !nt.starts_with(r"\Device\") || nt.contains('\0') {
        return Err(invalid("snapshot handle has no final NT device path"));
    }
    Ok(format!(r"\\?\GLOBALROOT{nt}"))
}

fn validate_child_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\', '\0'])
        || name.encode_utf16().count() > 255
    {
        return Err(invalid(
            "snapshot child must be one bounded relative component",
        ));
    }
    crate::platform::validate_relative_path(name)
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ChildName {
    name: String,
    directory: bool,
}

fn u32_at(bytes: &[u8], offset: usize) -> io::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("short native metadata record"))?;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
}

fn i64_at(bytes: &[u8], offset: usize) -> io::Result<i64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| invalid("short native metadata record"))?;
    Ok(i64::from_le_bytes(value.try_into().expect("eight bytes")))
}

fn utf16_field(bytes: &[u8], offset: usize, length: usize) -> io::Result<String> {
    if length == 0 || length % 2 != 0 {
        return Err(invalid("invalid native filename length"));
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| invalid("native name length overflow"))?;
    let value = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("native name exceeds its record"))?;
    let units: Vec<u16> = value
        .chunks_exact(2)
        .map(|part| u16::from_le_bytes([part[0], part[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| invalid("non-Unicode snapshot name"))
}

fn parse_directory_buffer(bytes: &[u8], children: &mut Vec<ChildName>) -> io::Result<()> {
    let mut offset = 0;
    loop {
        let record = bytes
            .get(offset..)
            .ok_or_else(|| invalid("directory record outside buffer"))?;
        let next = u32_at(record, 0)? as usize;
        let length = u32_at(record, 60)? as usize;
        let end = 68usize
            .checked_add(length)
            .ok_or_else(|| invalid("directory name overflow"))?;
        if end > record.len() || (next != 0 && (next % 8 != 0 || next < end || next > record.len()))
        {
            return Err(invalid("invalid native directory record bounds"));
        }
        let name = utf16_field(record, 68, length)?;
        if name != "." && name != ".." {
            validate_child_name(&name)?;
            if children.len() >= TREE_ENTRIES || u32_at(record, 64)? != 0 {
                return Err(invalid(
                    "directory exceeds entry bound or has unsupported extended attributes",
                ));
            }
            let attributes = u32_at(record, 56)?;
            if attributes & ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(invalid(
                    "reparse entries are outside the snapshot tree contract",
                ));
            }
            children.push(ChildName {
                name,
                directory: attributes & ATTRIBUTE_DIRECTORY != 0,
            });
        }
        if next == 0 {
            return Ok(());
        }
        offset += next;
    }
}

fn directory_children(file: &File) -> io::Result<Vec<ChildName>> {
    // FILE_FULL_DIR_INFO requires eight-byte alignment. Its filename begins at
    // byte 68; parse explicit lengths, never a speculative NUL terminator.
    let mut buffer = vec![0u64; QUERY_BYTES / 8];
    let mut children = Vec::new();
    let mut restart = true;
    loop {
        buffer.fill(0);
        // SAFETY: owned directory handle, correctly aligned live output buffer.
        let success = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                if restart { 15 } else { 14 },
                buffer.as_mut_ptr().cast(),
                QUERY_BYTES as u32,
            )
        };
        if success == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(18) {
                break;
            }
            return Err(error);
        }
        restart = false;
        // SAFETY: the buffer owns QUERY_BYTES initialized bytes for this scope.
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), QUERY_BYTES) };
        parse_directory_buffer(bytes, &mut children)?;
    }
    children.sort();
    let mut names = BTreeSet::new();
    for child in &children {
        // A case-sensitive source must not silently collapse when materialized
        // into the ordinary case-insensitive Windows writable destination.
        if !names.insert(child.name.to_uppercase()) {
            return Err(invalid("duplicate or case-alias snapshot child name"));
        }
    }
    Ok(children)
}

fn validate_tree_attributes(attributes: u32, directory: bool, links: u32) -> io::Result<()> {
    // Archive/normal/directory and NOT_CONTENT_INDEXED are accepted. The latter
    // is an indexing hint, not preserved in canonical metadata (like archive).
    // Raw attributes must still stay identical across capture. Reject rather
    // than silently dropping hidden/system/readonly, sparse/compressed/encrypted,
    // offline/recall, reparse, or any unknown storage semantics. ACLs and access
    // times remain outside Sprig's canonical metadata contract, as elsewhere.
    const ACCEPTED: u32 = ATTRIBUTE_DIRECTORY | 0x20 | 0x80 | 0x2000;
    if attributes & !ACCEPTED != 0
        || (attributes & ATTRIBUTE_DIRECTORY != 0) != directory
        || links == 0
    {
        return Err(invalid("unsupported snapshot attributes or link metadata"));
    }
    Ok(())
}

fn query_file_information(file: &File, class: u32, buffer: &mut [u64]) -> io::Result<usize> {
    let mut status = IoStatusBlock::default();
    // SAFETY: the file remains owned; the output is live, initialized and aligned.
    // Synchronous metadata queries require no completion callback or event.
    nt_result(unsafe {
        NtQueryInformationFile(
            file.as_raw_handle(),
            &mut status,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(buffer) as u32,
            class,
        )
    })?;
    if status.information > std::mem::size_of_val(buffer) {
        return Err(invalid("native file information exceeds its output buffer"));
    }
    Ok(status.information)
}

fn validate_stream_buffer(bytes: &[u8], directory: bool, size: u64) -> io::Result<()> {
    if directory && bytes.is_empty() {
        return Ok(());
    }
    // Only the one unnamed default data stream is represented by a file Entry.
    // An empty default stream is still required for an empty regular file.
    if directory
        || u32_at(bytes, 0)? != 0
        || utf16_field(bytes, 24, u32_at(bytes, 4)? as usize)? != "::$DATA"
        || i64_at(bytes, 8)? < 0
        || i64_at(bytes, 8)? as u64 != size
    {
        return Err(invalid(
            "named, missing, or inconsistent snapshot data stream",
        ));
    }
    Ok(())
}

fn validate_tree_metadata(node: &Node) -> io::Result<()> {
    validate_tree_attributes(
        node.before.attributes,
        node.before.directory,
        node.before.links,
    )?;
    if node.before.directory {
        let mut flags = 0u32;
        // SAFETY: the owned directory and FILE_CASE_SENSITIVE_INFO output remain
        // live. Case-sensitive directory semantics are not recreated by Sprig.
        if unsafe {
            GetFileInformationByHandleEx(
                node.file.as_raw_handle(),
                23,
                (&mut flags as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if flags != 0 {
            return Err(invalid("case-sensitive directory metadata is unsupported"));
        }
    }
    let mut ea = [0u64; 1];
    let length = query_file_information(&node.file, 7, &mut ea)?; // FileEaInformation
    if length != 4 || ea[0] & u64::from(u32::MAX) != 0 {
        return Err(invalid(
            "extended attributes are outside the snapshot tree contract",
        ));
    }
    let mut streams = vec![0u64; QUERY_BYTES / 8];
    let length = query_file_information(&node.file, 22, &mut streams)?; // FileStreamInformation
    // SAFETY: only the initialized returned prefix of the aligned output is read.
    let bytes = unsafe { std::slice::from_raw_parts(streams.as_ptr().cast::<u8>(), length) };
    validate_stream_buffer(bytes, node.before.directory, node.before.size)
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
    fn open_relative(parent: &Node, name: &str, directory: bool, serial: u32) -> io::Result<Self> {
        validate_child_name(name)?;
        if !parent.before.directory {
            return Err(invalid("relative snapshot parent must be a directory"));
        }
        let mut units: Vec<u16> = name.encode_utf16().collect();
        let mut unicode = UnicodeString {
            length: (units.len() * 2) as u16,
            maximum_length: (units.len() * 2) as u16,
            buffer: units.as_mut_ptr(),
        };
        let mut attributes = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root: parent.file.as_raw_handle(),
            name: &mut unicode,
            attributes: OBJ_DONT_REPARSE,
            security: std::ptr::null_mut(),
            quality_of_service: std::ptr::null_mut(),
        };
        let mut status = IoStatusBlock::default();
        let mut raw = std::ptr::null_mut();
        // SAFETY: parent owns its retained directory handle; exactly one checked
        // relative component and all NT ABI inputs remain live. NtOpenFile never
        // creates anything. No reparse following, writes, inheritance, or sharing
        // of write/delete is requested. The output is a new uniquely owned handle.
        nt_result(unsafe {
            NtOpenFile(
                &mut raw,
                FILE_GENERIC_READ,
                &mut attributes,
                &mut status,
                SHARE_READ,
                NT_OPEN_REPARSE_POINT
                    | NT_SYNCHRONOUS_NONALERT
                    | if directory {
                        NT_DIRECTORY_FILE
                    } else {
                        NT_NON_DIRECTORY_FILE
                    },
            )
        })?;
        if raw.is_null() || raw == INVALID_HANDLE {
            return Err(invalid("native relative open returned an invalid handle"));
        }
        // SAFETY: only a successful NtOpenFile's fresh output reaches this point.
        let file = unsafe { File::from_raw_handle(raw) };
        let before = snapshot(&file, serial)?;
        if before.directory != directory || before.links == 0 {
            return Err(invalid(
                "relative snapshot node has the wrong type or no links",
            ));
        }
        let path = format!("{}\\{name}", parent.path.trim_end_matches('\\'));
        Ok(Self { file, path, before })
    }

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

#[derive(Clone, Debug, Eq, PartialEq)]
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
struct ObjectBasicInfo {
    attributes: u32,
    granted_access: u32,
    handle_count: u32,
    pointer_count: u32,
    reserved: [u32; 10],
}
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: *mut c_void,
    name: *mut UnicodeString,
    attributes: u32,
    security: *mut c_void,
    quality_of_service: *mut c_void,
}
#[repr(C)]
#[derive(Default)]
struct IoStatusBlock {
    // The first member is an NTSTATUS/pointer union, hence pointer-sized.
    status_or_pointer: usize,
    information: usize,
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
    assert!(std::mem::size_of::<ObjectBasicInfo>() == 56);
    assert!(std::mem::offset_of!(ObjectBasicInfo, granted_access) == 4);
    assert!(std::mem::size_of::<IoStatusBlock>() == 2 * std::mem::size_of::<usize>());
    assert!(std::mem::offset_of!(ObjectAttributes, root) == std::mem::size_of::<usize>());
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
    fn GetFinalPathNameByHandleW(
        handle: *mut c_void,
        path: *mut u16,
        length: u32,
        flags: u32,
    ) -> u32;
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

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryObject(
        handle: *mut c_void,
        class: u32,
        info: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtOpenFile(
        handle: *mut *mut c_void,
        access: u32,
        attributes: *mut ObjectAttributes,
        status: *mut IoStatusBlock,
        share: u32,
        options: u32,
    ) -> i32;
    fn NtQueryInformationFile(
        handle: *mut c_void,
        status: *mut IoStatusBlock,
        info: *mut c_void,
        size: u32,
        class: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata(directory: bool) -> Snapshot {
        Snapshot {
            volume: 42,
            identifier: [1; 16],
            size: 0,
            links: 1,
            attributes: if directory { ATTRIBUTE_DIRECTORY } else { 0x20 },
            directory,
            creation: EPOCH_TICKS as i64,
            write: EPOCH_TICKS as i64,
            change: EPOCH_TICKS as i64,
        }
    }

    fn directory_record(name: &str, attributes: u32) -> Vec<u8> {
        let name: Vec<u16> = name.encode_utf16().collect();
        let mut record = vec![0; 68 + 2 * name.len()];
        record[56..60].copy_from_slice(&attributes.to_le_bytes());
        record[60..64].copy_from_slice(&(2 * name.len() as u32).to_le_bytes());
        for (bytes, unit) in record[68..].chunks_exact_mut(2).zip(name) {
            bytes.copy_from_slice(&unit.to_le_bytes());
        }
        record
    }

    #[test]
    fn tree_capability_scope_and_resource_bounds_are_fail_closed() {
        assert!(nt_result(0).is_ok());
        assert!(nt_result(0x103).is_err()); // STATUS_PENDING
        assert!(nt_result(0x4000_0000).is_err()); // Informational, not success.
        let root = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\fixture";
        assert!(validate_root_binding(root, root).is_ok());
        for other in [
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy2\fixture",
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\fixture\src",
            r"C:\fixture",
        ] {
            assert!(validate_root_binding(other, root).is_err());
            assert!(validate_root_binding(root, other).is_err());
        }
        for accepted in [FILE_GENERIC_READ, FILE_READ_DATA_ATTRIBUTES] {
            assert!(validate_granted_access(accepted).is_ok());
        }
        for denied in [
            0,
            0x80,
            GENERIC_READ,
            0x1000_0000,
            0x0002,
            0x0004,
            0x0010,
            0x0020,
            0x0040,
            0x0100,
            0x0001_0000,
            0x0004_0000,
            0x0008_0000,
        ] {
            assert!(validate_granted_access(denied).is_err());
            if denied != 0 && denied != 0x80 {
                assert!(validate_granted_access(FILE_GENERIC_READ | denied).is_err());
            }
        }
        for name in [
            "",
            ".",
            "..",
            "a/b",
            r"a\b",
            "a:ads",
            "NUL.txt",
            "trailing.",
            "a\0b",
        ] {
            assert!(validate_child_name(name).is_err(), "{name:?}");
        }
        assert!(validate_child_name("Cargo.toml").is_ok());
        assert!(validate_child_name("模块.rs").is_ok());
        let mut budget = TreeBudget::default();
        budget.add(0, true, u64::MAX).unwrap();
        budget.add(TREE_DEPTH, false, TREE_BYTES).unwrap();
        assert!(budget.add(1, false, 1).is_err());
        assert!(budget.add(TREE_DEPTH + 1, true, 0).is_err());
        assert_eq!(budget.bytes, TREE_BYTES);
        for _ in 2..TREE_ENTRIES {
            budget.add(1, true, 0).unwrap();
        }
        assert!(budget.add(1, true, 0).is_err());
        assert!(
            TreeBudget {
                entries: 0,
                bytes: u64::MAX
            }
            .add(0, false, 1)
            .is_err()
        );
        for attributes in [
            ATTRIBUTE_READONLY,
            0x2,
            0x4,
            0x100,
            0x200,
            0x400,
            0x800,
            0x1000,
            0x4000,
            0x40000,
            0x10000000,
        ] {
            assert!(validate_tree_attributes(attributes, false, 1).is_err());
        }
        assert!(validate_tree_attributes(0x20, false, 1).is_ok());
        assert!(validate_tree_attributes(0x2020, false, 1).is_ok());
        assert!(validate_tree_attributes(0x2010, true, 1).is_ok());
        assert!(validate_tree_attributes(ATTRIBUTE_DIRECTORY, true, 1).is_ok());
        assert!(validate_tree_attributes(0x20, false, 0).is_err());
    }

    #[test]
    fn native_tree_records_and_hard_link_groups_are_validated() {
        let record = directory_record("Cargo.toml", 0x20);
        let mut children = Vec::new();
        parse_directory_buffer(&record, &mut children).unwrap();
        assert_eq!(
            children,
            [ChildName {
                name: "Cargo.toml".to_owned(),
                directory: false
            }]
        );
        for bad in [
            record[..record.len() - 1].to_vec(),
            directory_record("../outside", 0),
            directory_record("link", ATTRIBUTE_REPARSE_POINT),
        ] {
            assert!(parse_directory_buffer(&bad, &mut Vec::new()).is_err());
        }
        for (offset, value) in [(0, 1u32), (0, u32::MAX), (60, u32::MAX), (64, 1)] {
            let mut bad = record.clone();
            bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(parse_directory_buffer(&bad, &mut Vec::new()).is_err());
        }
        let mut stream = vec![0; 38];
        stream[4..8].copy_from_slice(&14u32.to_le_bytes());
        for (bytes, unit) in stream[24..]
            .chunks_exact_mut(2)
            .zip("::$DATA".encode_utf16())
        {
            bytes.copy_from_slice(&unit.to_le_bytes());
        }
        assert!(validate_stream_buffer(&stream, false, 0).is_ok());
        assert!(validate_stream_buffer(&[], true, 0).is_ok());
        assert!(validate_stream_buffer(&[], false, 0).is_err());
        assert!(validate_stream_buffer(&stream, true, 0).is_err());
        assert!(validate_stream_buffer(&stream, false, 1).is_err());
        let mut ads = stream.clone();
        ads[26] = b'x';
        assert!(validate_stream_buffer(&ads, false, 0).is_err());
        stream[0..4].copy_from_slice(&40u32.to_le_bytes());
        assert!(validate_stream_buffer(&stream, false, 0).is_err());

        let mut directory = sample_metadata(true);
        directory.identifier = [0; 16];
        let mut file = sample_metadata(false);
        file.links = 2;
        assert!(validate_tree_links([&directory, &file, &file].into_iter()).is_ok());
        assert!(validate_tree_links([&directory, &file].into_iter()).is_err());
        assert!(validate_tree_links([&directory, &directory].into_iter()).is_err());
        let mut changed = file.clone();
        changed.write += 1;
        assert!(validate_tree_links([&file, &changed].into_iter()).is_err());
        let mut manifest = Manifest::new();
        let mut observed = content::Observed::new();
        for (path, metadata) in [("", &directory), ("a", &file), ("b", &file)] {
            let kind = if metadata.directory {
                EntryKind::Directory
            } else {
                EntryKind::File {
                    size: 0,
                    chunks: vec![],
                }
            };
            let value = entry("root", "token", path, metadata, kind);
            observed.insert(
                path.to_owned(),
                content::Observation {
                    id: value.id.clone(),
                    link_group: None,
                    device: metadata.volume,
                    inode: u128::from_le_bytes(metadata.identifier),
                },
            );
            manifest.insert(path.to_owned(), value);
        }
        content::assign_link_groups(&mut manifest, &mut observed, &content::Observed::new())
            .unwrap();
        assert_ne!(manifest["a"].id, manifest["b"].id);
        assert_eq!(manifest["a"].link_group.as_ref(), Some(&manifest["a"].id));
        assert_eq!(manifest["a"].link_group, manifest["b"].link_group);
        assert_eq!(manifest[""].link_group, None);
    }

    #[test]
    fn local_windows_handles_are_relative_read_only_and_not_vss_authority() -> crate::Result<()> {
        use std::fs::{self, OpenOptions};
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};
        struct Temporary(std::path::PathBuf);
        impl Drop for Temporary {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "sprig-vss-handles-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&path)?;
        let temporary = Temporary(path.canonicalize()?);
        let source = temporary.0.join("source");
        fs::create_dir(&source)?;
        fs::create_dir(source.join("src"))?;
        fs::write(source.join("a"), b"bounded")?;
        fs::hard_link(source.join("a"), source.join("b"))?;
        fs::write(source.join("ads"), b"ordinary")?;
        fs::write(source.join("ads:extra"), b"not represented")?;
        let root_file = OpenOptions::new()
            .access_mode(GENERIC_READ)
            .share_mode(SHARE_READ)
            .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
            .open(&source)?;
        require_read_only_handle(&root_file)?;
        let mut serial = 0;
        // SAFETY: owned live handle; only the requested serial output is nonnull.
        if unsafe {
            GetVolumeInformationByHandleW(
                root_file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let root = Node {
            before: snapshot(&root_file, serial)?,
            file: root_file,
            path: source
                .to_str()
                .expect("Unicode temporary directory")
                .to_owned(),
        };
        validate_tree_metadata(&root)?;
        assert_eq!(
            directory_children(&root.file)?
                .iter()
                .map(|child| child.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "ads", "b", "src"]
        );
        let a = Node::open_relative(&root, "a", false, serial)?;
        let b = Node::open_relative(&root, "b", false, serial)?;
        assert_eq!(a.before, b.before);
        validate_tree_links([&a.before, &b.before].into_iter())?;
        assert!(validate_tree_links([&a.before].into_iter()).is_err());
        validate_tree_metadata(&a)?;
        assert!(
            validate_tree_metadata(&Node::open_relative(&root, "ads", false, serial)?).is_err()
        );
        assert!(Node::open_relative(&root, "../a", false, serial).is_err());
        assert!(Node::open_relative(&root, "src", false, serial).is_err());
        assert!(Node::open_relative(&root, "a", true, serial).is_err());
        assert_eq!(read_bounded(&a, 7)?, b"bounded");
        a.recheck(serial)?;

        let store_path = temporary.0.join("store");
        let store = crate::Store::init(&store_path)?;
        let state = fs::read(store_path.join("state.json"))?;
        let expected = Path::new(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\fixture");
        let token = "0123456789abcdef0123456789abcdef";
        assert!(
            import_tree(
                &store,
                root.file.try_clone()?.into(),
                expected,
                "ordinary",
                serial,
                token
            )
            .is_err()
        );
        assert!(
            import_tree(
                &store,
                a.file.try_clone()?.into(),
                expected,
                "file",
                serial,
                token
            )
            .is_err()
        );
        let writable = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(temporary.0.join("writable"))?;
        assert!(require_read_only_handle(&writable).is_err());
        assert!(import_tree(&store, writable.into(), expected, "writable", serial, token).is_err());
        let mut reader = std::ptr::null_mut();
        let mut writer = std::ptr::null_mut();
        // SAFETY: live writable output slots; no inheritance/security is requested.
        if unsafe { CreatePipe(&mut reader, &mut writer, std::ptr::null(), 0) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: CreatePipe succeeded and returned two fresh owned handles.
        let reader = unsafe { OwnedHandle::from_raw_handle(reader) };
        let _writer = unsafe { OwnedHandle::from_raw_handle(writer) };
        assert!(import_tree(&store, reader, expected, "pipe", serial, token).is_err());
        assert_eq!(state, fs::read(store_path.join("state.json"))?);
        assert!(store.list()?.workspaces.is_empty());
        Ok(())
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreatePipe(
            reader: *mut *mut c_void,
            writer: *mut *mut c_void,
            security: *const c_void,
            size: u32,
        ) -> i32;
    }

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
