//! Immutable payload packs with an atomically replaced, bounded index.
//!
//! A snapshot owns exact pack locations, not a mutable process-wide cache.
//! Packs are never modified. Publication/compaction never removes old packs;
//! only explicit, reader-excluded reclamation can remove unreachable payloads.
//! A failed publication can leave an unindexed pack or an already-published
//! index; it never rolls either back.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::content::Result;
use crate::platform;

const INDEX_NAME: &str = "pack-index";
const LOCK_NAME: &str = ".pack-lock";
const READER_LOCK_NAME: &str = ".reader-lock";
const COMPRESSION_NAME: &str = ".compression";
const FORMAT_NAME: &str = ".pack-format";
const FORMAT_BYTES: &[u8] = b"sprig immutable packs 2\n";
const MAGIC: &[u8; 8] = b"SPRGPKIX";
const VERSION: u32 = 2;
const HEADER_BYTES: usize = 8 + 4 + 8 + 32;
const PACK_RECORD_BYTES: usize = 32 + 8;
const OBJECT_RECORD_BYTES: usize = 32 + 4 + 8 + 4 + 4 + 1;
const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHUNK_BYTES: u64 = 256 * 1024;
const CODEC_WINDOW_LOG: u32 = 18;
const COMPACTION_PACK_BYTES: u64 = 64 * 1024 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Codec {
    Raw = 0,
    Zstd = 1,
}

impl Codec {
    fn decode(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Zstd),
            _ => invalid("unsupported packed object codec"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Location {
    pub(crate) pack: String,
    pub(crate) offset: u64,
    /// Decoded bytes, whose BLAKE3 hash is the object address.
    pub(crate) length: u64,
    pub(crate) stored_length: u64,
    pub(crate) codec: Codec,
    pub(crate) pack_length: u64,
}

/// Structurally validated fixed-width index bytes. Opening a snapshot still
/// reads, checksums and validates the entire bounded index, but does not build
/// per-object Strings or trees. Selected addresses use binary search. Actual
/// payload length and content are checked again on every object read.
#[derive(Clone, Debug, Default)]
pub(crate) struct Snapshot {
    bytes: Vec<u8>,
    pack_count: usize,
    object_count: usize,
}

impl Snapshot {
    /// The opener must reject symlinks and open children of one pinned or
    /// otherwise trusted object directory. A missing index means loose-only.
    pub(crate) fn load_with(mut open: impl FnMut(&str) -> io::Result<File>) -> Result<Self> {
        let file = match open(INDEX_NAME) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if read_format(&mut open)? {
                    // The first writer may have published both between our
                    // first index open and marker open. Index publication is
                    // ordered before the marker, so one retry is sufficient.
                    match open(INDEX_NAME) {
                        Ok(file) => file,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            return invalid("packed object store is missing its index");
                        }
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    return Ok(Self::default());
                }
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = platform::file_metadata(&file)?;
        if !metadata.is_file()
            || metadata.len() < HEADER_BYTES as u64
            || metadata.len() > MAX_INDEX_BYTES as u64
        {
            return invalid("pack index is not a regular file within the 64 MiB limit");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_INDEX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != metadata.len() {
            return invalid("pack index changed length while reading");
        }
        let snapshot = decode_index(bytes)?;
        if !read_format(&mut open)? && snapshot.pack_count != 0 {
            return invalid("packed object store is missing its format marker");
        }
        Ok(snapshot)
    }

    pub(crate) fn read_with(
        &self,
        hash: &str,
        mut open: impl FnMut(&str) -> io::Result<File>,
    ) -> Result<Vec<u8>> {
        // An indexed address always selects the exact immutable pack. A loose
        // alias cannot hide pack corruption or change an existing snapshot.
        if let Some(location) = self.location(hash)? {
            read_range(open(&location.pack)?, hash, &location)
        } else {
            read_loose(open(hash)?, hash)
        }
    }

    pub(crate) fn length_with(
        &self,
        hash: &str,
        mut open: impl FnMut(&str) -> io::Result<File>,
    ) -> Result<u64> {
        // Metadata-only lookup for projection planning. Content is verified
        // by read_with before use and by every deduplication attempt.
        if let Some(location) = self.location(hash)? {
            validate_location(&location)?;
            let metadata = platform::file_metadata(&open(&location.pack)?)?;
            if !metadata.is_file() || metadata.len() != location.pack_length {
                return invalid(format!("pack payload length mismatch: {}", location.pack));
            }
            Ok(location.length)
        } else {
            let metadata = platform::file_metadata(&open(hash)?)?;
            if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CHUNK_BYTES {
                return invalid(format!("invalid loose chunk length for {hash}"));
            }
            Ok(metadata.len())
        }
    }

    fn pack_records(&self) -> &[[u8; PACK_RECORD_BYTES]] {
        if self.bytes.is_empty() {
            return &[];
        }
        let start = HEADER_BYTES + 8;
        self.bytes[start..start + self.pack_count * PACK_RECORD_BYTES]
            .as_chunks::<PACK_RECORD_BYTES>()
            .0
    }

    fn object_records(&self) -> &[[u8; OBJECT_RECORD_BYTES]] {
        if self.bytes.is_empty() {
            return &[];
        }
        let start = HEADER_BYTES + 8 + self.pack_count * PACK_RECORD_BYTES;
        self.bytes[start..].as_chunks::<OBJECT_RECORD_BYTES>().0
    }

    fn location(&self, hash: &str) -> Result<Option<Location>> {
        let address = hash_bytes(hash)?;
        let records = self.object_records();
        Ok(records
            .binary_search_by(|record| record[..32].cmp(&address[..]))
            .ok()
            .map(|index| self.record_location(&records[index])))
    }

    fn record_location(&self, record: &[u8; OBJECT_RECORD_BYTES]) -> Location {
        // These fixed fields and the referenced pack were fully checked when
        // this immutable snapshot was loaded; no unchecked input enters here.
        let number = u32::from_le_bytes(record[32..36].try_into().expect("fixed field"));
        let pack = &self.pack_records()[number as usize];
        Location {
            pack: format!("pack-{}", hex(pack[..32].try_into().expect("fixed field"))),
            offset: u64::from_le_bytes(record[36..44].try_into().expect("fixed field")),
            length: u32::from_le_bytes(record[44..48].try_into().expect("fixed field")) as u64,
            stored_length: u32::from_le_bytes(record[48..52].try_into().expect("fixed field"))
                as u64,
            codec: Codec::decode(record[52]).expect("validated codec"),
            pack_length: u64::from_le_bytes(pack[32..40].try_into().expect("fixed field")),
        }
    }

    /// Catalog expansion is reserved for inventory/GC and writer publication,
    /// not ordinary reads. Each returned map is owned by its caller.
    pub(crate) fn locations(&self) -> BTreeMap<String, Location> {
        self.object_records()
            .iter()
            .map(|record| {
                (
                    hex(record[..32].try_into().expect("fixed field")),
                    self.record_location(record),
                )
            })
            .collect()
    }

    /// Packed entries only. Callers can separately enumerate legacy loose
    /// objects. These are logical sizes, not individually reclaimable bytes.
    pub(crate) fn object_sizes(&self) -> BTreeMap<String, u64> {
        self.object_records()
            .iter()
            .map(|record| {
                (
                    hex(record[..32].try_into().expect("fixed field")),
                    u32::from_le_bytes(record[44..48].try_into().expect("fixed field")) as u64,
                )
            })
            .collect()
    }

    pub(crate) fn pack_sizes(&self) -> BTreeMap<String, u64> {
        self.pack_records()
            .iter()
            .map(|record| {
                (
                    format!(
                        "pack-{}",
                        hex(record[..32].try_into().expect("fixed field"))
                    ),
                    u64::from_le_bytes(record[32..40].try_into().expect("fixed field")),
                )
            })
            .collect()
    }
}

#[derive(Default)]
struct Catalog {
    locations: BTreeMap<String, Location>,
    packs: BTreeMap<String, u64>,
}

pub(crate) struct Reader {
    _lease: ReadLease,
    objects: PathBuf,
    snapshot: Snapshot,
}

impl Reader {
    pub(crate) fn open(objects: &Path) -> Result<Self> {
        validate_directory_chain(objects)?;
        let lease = ReadLease::acquire(objects)?;
        Ok(Self {
            _lease: lease,
            objects: objects.to_owned(),
            snapshot: Snapshot::load_with(|name| platform::open_regular(&objects.join(name)))?,
        })
    }

    pub(crate) fn read(&self, hash: &str) -> Result<Vec<u8>> {
        self.snapshot.read_with(hash, |name| {
            platform::open_regular(&self.objects.join(name))
        })
    }

    pub(crate) fn length(&self, hash: &str) -> Result<u64> {
        self.snapshot.length_with(hash, |name| {
            platform::open_regular(&self.objects.join(name))
        })
    }

    pub(crate) fn locations(&self) -> BTreeMap<String, Location> {
        self.snapshot.locations()
    }

    pub(crate) fn object_sizes(&self) -> BTreeMap<String, u64> {
        self.snapshot.object_sizes()
    }

    pub(crate) fn pack_sizes(&self) -> BTreeMap<String, u64> {
        self.snapshot.pack_sizes()
    }
}

/// Requested-content integrity for ordinary point reads, not an index audit.
/// The index file is pinned once; bounded probes compare addresses, and only
/// a matching record has its pack reference and extent validated. Successful
/// reads always hash against the caller's original
/// address. The global checksum, ordering, duplicates and unvisited records
/// are deliberately not audited here. Reader/Snapshot retain that contract.
pub(crate) struct PointReader {
    _lease: ReadLease,
    objects: PathBuf,
    index: Option<PointIndex>,
}

impl PointReader {
    pub(crate) fn open(objects: &Path) -> Result<Self> {
        validate_directory_chain(objects)?;
        let lease = ReadLease::acquire(objects)?;
        let mut open = |name: &str| platform::open_regular(&objects.join(name));
        let file = match open(INDEX_NAME) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if read_format(&mut open)? {
                    // Match the initialization race handling of Snapshot:
                    // the index must precede a newly visible format marker.
                    match open(INDEX_NAME) {
                        Ok(file) => Some(file),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            return invalid("packed object store is missing its index");
                        }
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    None
                }
            }
            Err(error) => return Err(error.into()),
        };
        let index = file.map(PointIndex::open).transpose()?;
        if let Some(index) = &index
            && !read_format(&mut open)?
            && index.pack_count != 0
        {
            return invalid("packed object store is missing its format marker");
        }
        Ok(Self {
            _lease: lease,
            objects: objects.to_owned(),
            index,
        })
    }

    pub(crate) fn read(&mut self, hash: &str) -> Result<Vec<u8>> {
        let address = hash_bytes(hash)?;
        if let Some(index) = &mut self.index
            && let Some(location) = index.location(&address)?
        {
            // Never substitute the probed record's address for the caller's
            // hash. Even a corrupted index cannot authorize different bytes.
            return read_range(
                platform::open_regular(&self.objects.join(&location.pack))?,
                hash,
                &location,
            );
        }
        // Corrupt global ordering may produce a false miss. This fallback
        // still independently verifies loose bytes against the original hash.
        read_loose(platform::open_regular(&self.objects.join(hash))?, hash)
    }
}

struct PointIndex {
    file: File,
    length: u64,
    pack_count: usize,
    object_count: usize,
    object_start: u64,
}

impl PointIndex {
    fn open(mut file: File) -> Result<Self> {
        let metadata = platform::file_metadata(&file)?;
        if !metadata.is_file()
            || metadata.len() < (HEADER_BYTES + 8) as u64
            || metadata.len() > MAX_INDEX_BYTES as u64
        {
            return invalid("pack index is not a regular file within the 64 MiB limit");
        }
        let mut header = [0; HEADER_BYTES + 8];
        file.read_exact(&mut header)?;
        let mut cursor = header.as_slice();
        if field::<8>(&mut cursor)? != *MAGIC || u32::from_le_bytes(field(&mut cursor)?) != VERSION
        {
            return invalid("unsupported pack index format or version");
        }
        let body_length = u64::from_le_bytes(field(&mut cursor)?);
        // Point queries intentionally do not scan the body for this checksum.
        let _checksum = field::<32>(&mut cursor)?;
        let pack_count = u32::from_le_bytes(field(&mut cursor)?) as usize;
        let object_count = u32::from_le_bytes(field(&mut cursor)?) as usize;
        if body_length != metadata.len() - HEADER_BYTES as u64
            || index_size(pack_count, object_count)? as u64 != metadata.len()
            || (object_count != 0 && pack_count == 0)
        {
            return invalid("pack index record counts do not match its length");
        }
        Ok(Self {
            file,
            length: metadata.len(),
            pack_count,
            object_count,
            object_start: (HEADER_BYTES + 8 + pack_count * PACK_RECORD_BYTES) as u64,
        })
    }

    fn location(&mut self, address: &[u8; 32]) -> Result<Option<Location>> {
        if platform::file_metadata(&self.file)?.len() != self.length {
            return invalid("pinned pack index changed length");
        }
        let mut low = 0;
        let mut high = self.object_count;
        // Every iteration shrinks the interval. The explicit cap additionally
        // bounds work even for malformed global ordering or future changes.
        for _ in 0..usize::BITS {
            if low >= high {
                return Ok(None);
            }
            let middle = low + (high - low) / 2;
            let record = self.record::<OBJECT_RECORD_BYTES>(self.object_start, middle)?;
            match record[..32].cmp(&address[..]) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let number = u32::from_le_bytes(record[32..36].try_into().expect("fixed field"))
                        as usize;
                    if number >= self.pack_count {
                        return invalid("object refers to an unknown pack");
                    }
                    let pack =
                        self.record::<PACK_RECORD_BYTES>((HEADER_BYTES + 8) as u64, number)?;
                    let offset =
                        u64::from_le_bytes(record[36..44].try_into().expect("fixed field"));
                    let length =
                        u32::from_le_bytes(record[44..48].try_into().expect("fixed field")) as u64;
                    let stored_length =
                        u32::from_le_bytes(record[48..52].try_into().expect("fixed field")) as u64;
                    let codec = Codec::decode(record[52])?;
                    let pack_length =
                        u64::from_le_bytes(pack[32..40].try_into().expect("fixed field"));
                    validate_descriptor(offset, length, stored_length, codec, pack_length)?;
                    return Ok(Some(Location {
                        pack: format!("pack-{}", hex(pack[..32].try_into().expect("fixed field"))),
                        offset,
                        length,
                        stored_length,
                        codec,
                        pack_length,
                    }));
                }
            }
        }
        invalid("pack index point lookup exceeded its probe bound")
    }

    fn record<const N: usize>(&mut self, start: u64, number: usize) -> Result<[u8; N]> {
        let offset = (number as u64)
            .checked_mul(N as u64)
            .and_then(|offset| start.checked_add(offset))
            .ok_or_else(|| invalid_error("pack index record offset overflow"))?;
        if offset
            .checked_add(N as u64)
            .is_none_or(|end| end > self.length)
        {
            return invalid("pack index record is outside the pinned file");
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut record = [0; N];
        self.file.read_exact(&mut record)?;
        Ok(record)
    }
}

struct PendingPack {
    file: File,
    temporary: Temporary,
    name: String,
    length: u64,
}

/// One nonblocking, exclusively locked publication batch. Returned addresses
/// are not usable references until finish succeeds. A failed put poisons this
/// batch so callers cannot accidentally commit a partially written payload.
#[must_use = "finish the batch before publishing its object references"]
pub(crate) struct Batch {
    reader: Reader,
    pending: Option<PendingPack>,
    additions: BTreeMap<String, Location>,
    failed: bool,
    compression: bool,
    // Keep the lock inode and handle for the entire batch. Never unlink it.
    _lock: File,
}

impl Batch {
    /// The caller creates the object directory before opening a batch.
    pub(crate) fn open(objects: &Path) -> Result<Self> {
        validate_directory_chain(objects)?;
        let lock = writer_lock(objects)?;
        let compression = compression_enabled(objects)?;
        // Read only after taking the writer lock, to avoid lost index updates.
        Ok(Self {
            reader: Reader::open(objects)?,
            pending: None,
            additions: BTreeMap::new(),
            failed: false,
            compression,
            _lock: lock,
        })
    }

    pub(crate) fn put(&mut self, bytes: &[u8]) -> Result<String> {
        if self.failed {
            return invalid("object batch cannot continue after a failed put");
        }
        let result = self.put_inner(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn put_inner(&mut self, bytes: &[u8]) -> Result<String> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_CHUNK_BYTES {
            return invalid("packed chunk length must be within 1..=256 KiB");
        }
        let hash = blake3::hash(bytes).to_hex().to_string();
        if let Some(location) = self.additions.get(&hash) {
            let pending = self.pending.as_ref().expect("pending chunk has a payload");
            let mut location = location.clone();
            location.pack_length = pending.length;
            let existing = read_range(
                platform::open_regular(&pending.temporary.0)?,
                &hash,
                &location,
            )?;
            if existing != bytes {
                return invalid("pending chunk differs from its content address");
            }
            return Ok(hash);
        }
        let loose_exists = match platform::symlink_metadata(self.reader.objects.join(&hash)) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let packed_exists = self.reader.snapshot.location(&hash)?.is_some();
        if loose_exists || packed_exists {
            if self.reader.read(&hash)? != bytes {
                return invalid("existing chunk differs from its content address");
            }
            // If both layouts happen to contain this address, verify both.
            // Neither an old loose alias nor a packed copy may hide damage.
            if loose_exists
                && packed_exists
                && read_loose(
                    platform::open_regular(&self.reader.objects.join(&hash))?,
                    &hash,
                )? != bytes
            {
                return invalid("loose alias differs from its content address");
            }
            return Ok(hash);
        }
        let (codec, encoded) = encode_with_policy(bytes, self.compression)?;
        // A single raw chunk stays loose. A compressed singleton needs an
        // indexed descriptor too, so check the index bound before appending.
        if !self.additions.is_empty() || codec != Codec::Raw {
            let objects = self
                .reader
                .snapshot
                .object_count
                .checked_add(self.additions.len())
                .and_then(|count| count.checked_add(1))
                .ok_or_else(|| invalid_error("object count overflow"))?;
            let packs = self
                .reader
                .snapshot
                .pack_count
                .checked_add(1)
                .ok_or_else(|| invalid_error("pack count overflow"))?;
            index_size(packs, objects)?;
        }
        if self.pending.is_none() {
            self.pending = Some(new_pending(&self.reader.objects)?);
        }
        let pending = self.pending.as_mut().expect("payload created");
        let location = append_encoded(pending, bytes.len() as u64, codec, &encoded)?;
        self.additions.insert(hash.clone(), location);
        Ok(hash)
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        if self.failed {
            return invalid("object batch cannot finish after a failed put");
        }
        let Some(pending) = self.pending.take() else {
            // This also makes a previous uncertain publication durable when
            // retrying a batch that now only finds existing objects.
            platform::sync_directory(&self.reader.objects)?;
            return Ok(());
        };
        if self.additions.len() == 1 {
            let (hash, location) = self.additions.first_key_value().expect("one chunk");
            if location.codec == Codec::Raw {
                debug_assert_eq!((location.offset, location.length), (0, pending.length));
                return publish_loose(&self.reader.objects, pending, hash);
            }
        }
        let mut catalog = Catalog {
            locations: self.reader.snapshot.locations(),
            packs: self.reader.snapshot.pack_sizes(),
        };
        catalog.packs.insert(pending.name.clone(), pending.length);
        for (hash, mut location) in std::mem::take(&mut self.additions) {
            location.pack_length = pending.length;
            catalog.locations.insert(hash, location);
        }
        // Encoding, including the size cap, happens before any publication.
        let encoded = encode_index(&catalog)?;
        initialize_format(&self.reader.objects)?;
        publish_pack(&self.reader.objects, pending)?;
        publish_index(&self.reader.objects, &encoded)
    }
}

/// Shared reader lease, acquired before pinning any index generation. Low-level
/// Snapshot callers (including descriptor-relative mounts) must hold one too.
/// The lock inode is permanent: replacing or unlinking it would split the lock
/// domain and permit reclamation underneath an old reader.
#[derive(Debug)]
pub(crate) struct ReadLease {
    _file: File,
}

impl ReadLease {
    pub(crate) fn acquire(objects: &Path) -> Result<Self> {
        validate_directory_chain(objects)?;
        let file = permanent_lock(objects, READER_LOCK_NAME)?;
        file.try_lock_shared()
            .map_err(|error| io::Error::other(format!("object_store_reclaiming: {error}")))?;
        Ok(Self { _file: file })
    }
}

/// The caller must hold this guard throughout final reclamation and must also
/// exclude object writers / root mutations. It must not hold a Reader itself.
/// None means an old reader is alive: retain every retired object and retry
/// only as a separate, explicit reclamation operation. This never deletes data.
pub(crate) fn try_exclusive_readers(objects: &Path) -> Result<Option<File>> {
    validate_directory_chain(objects)?;
    let file = permanent_lock(objects, READER_LOCK_NAME)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn permanent_lock(objects: &Path, name: &str) -> io::Result<File> {
    let path = objects.join(name);
    match platform::create_new(&path, 0o600) {
        Ok(file) => {
            file.sync_all()?;
            platform::sync_directory(objects)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            platform::open_read_write(&path)
        }
        Err(error) => Err(error),
    }
}

fn writer_lock(objects: &Path) -> Result<File> {
    let lock = permanent_lock(objects, LOCK_NAME)?;
    lock.try_lock()
        .map_err(|error| io::Error::other(format!("object_store_busy: {error}")))?;
    Ok(lock)
}

#[derive(Debug)]
pub(crate) struct CompactionReport {
    /// Old indexed packs, now absent from the new index but still present on
    /// disk. This is not deletion authorization: old readers can reference them.
    pub(crate) retired_packs: BTreeMap<String, u64>,
    pub(crate) written_pack_bytes: u64,
    pub(crate) live_objects: u64,
    pub(crate) logical_bytes: u64,
}

/// Publish a verified, selectively compressed replacement for exactly these
/// addresses. The caller owns liveness: it must exclude root changes throughout
/// planning and this call. Loose live objects are included, never removed.
///
/// Payload packs are capped at 64 MiB. Peak disk includes ALL old packs plus
/// the new live packs and a replacement index. No deletion happens here, even
/// on failure. Before index replacement failures leave the old catalog intact;
/// after replacement an error may mean the new catalog is already committed.
/// Both outcomes leave the old reader generations usable.
pub(crate) fn compact(objects: &Path, live: &BTreeSet<String>) -> Result<CompactionReport> {
    compact_with_limit(objects, live, COMPACTION_PACK_BYTES)
}

#[derive(Debug, Default)]
pub(crate) struct ReclaimReport {
    pub(crate) removed_files: u64,
    pub(crate) removed_bytes: u64,
    pub(crate) readers_busy: bool,
}

/// Final, explicit reclamation after compaction. The caller must still hold
/// its root/liveness lock. The writer lock prevents a batch publication from
/// changing the checked catalog; the exclusive reader lease prevents deleting
/// files referenced by older index snapshots. Public batches not yet attached
/// to a live root are garbage under this contract, not implicit retention pins.
///
/// Refuse a changed live set rather than deleting against an obsolete plan.
/// Unknown names, temporary files, links, directories and permanent lock files
/// are never candidates. This function is not run by normal reads or writes.
pub(crate) fn reclaim_retired(objects: &Path, live: &BTreeSet<String>) -> Result<ReclaimReport> {
    validate_directory_chain(objects)?;
    let _writer = writer_lock(objects)?;
    let Some(_readers) = try_exclusive_readers(objects)? else {
        return Ok(ReclaimReport {
            readers_busy: true,
            ..ReclaimReport::default()
        });
    };
    let snapshot = Snapshot::load_with(|name| platform::open_regular(&objects.join(name)))?;
    let indexed: BTreeSet<String> = snapshot.object_sizes().into_keys().collect();
    if indexed != *live {
        return invalid("compaction live set changed before reclamation; replan required");
    }
    let packs = snapshot.pack_sizes();
    // Verify retained objects before removing aliases or older generations.
    // In particular, a damaged new pack must never justify deleting its only
    // surviving loose copy. Bounded chunk reads preserve the normal audit.
    for hash in live {
        snapshot.read_with(hash, |name| platform::open_regular(&objects.join(name)))?;
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(objects)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let retired_pack = validate_pack_name(name).is_ok() && !packs.contains_key(name);
        let loose = validate_hash(name).is_ok();
        if !retired_pack && !loose {
            continue;
        }
        let metadata = platform::symlink_metadata(entry.path())?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            candidates.push((entry.path(), metadata.len()));
        }
    }
    let mut report = ReclaimReport::default();
    for (path, bytes) in candidates {
        let next_bytes = report
            .removed_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid_error("reclaimed payload size overflow"))?;
        fs::remove_file(&path)?;
        report.removed_files += 1;
        report.removed_bytes = next_bytes;
    }
    platform::sync_directory(objects)?;
    Ok(report)
}

fn compact_with_limit(
    objects: &Path,
    live: &BTreeSet<String>,
    pack_limit: u64,
) -> Result<CompactionReport> {
    validate_directory_chain(objects)?;
    if pack_limit == 0 || pack_limit > COMPACTION_PACK_BYTES {
        return invalid("invalid compaction pack size limit");
    }
    // Bound catalog growth before allocating descriptors or writing payload.
    index_size(usize::from(!live.is_empty()), live.len())?;
    let _writer = writer_lock(objects)?;
    let compression = compression_enabled(objects)?;
    let reader = Reader::open(objects)?;
    let retired_packs = reader.pack_sizes();
    let mut catalog = Catalog::default();
    let mut pending: Option<PendingPack> = None;
    let mut pending_first = String::new();
    let mut logical_bytes = 0_u64;
    let mut written_pack_bytes = 0_u64;
    for hash in live {
        let bytes = reader.read(hash)?; // verifies hash and bounded decoded size
        let (codec, encoded) = encode_with_policy(&bytes, compression)?;
        if pending
            .as_ref()
            .is_some_and(|pack| pack.length != 0 && pack.length + encoded.len() as u64 > pack_limit)
        {
            let pack = pending.take().expect("current pack");
            finalize_compaction_pack(objects, &mut catalog, &pending_first, pack)?;
        }
        if pending.is_none() {
            index_size(catalog.packs.len() + 1, live.len())?;
            pending = Some(new_pending(objects)?);
            pending_first = hash.clone();
        }
        let location = append_encoded(
            pending.as_mut().expect("created pack"),
            bytes.len() as u64,
            codec,
            &encoded,
        )?;
        logical_bytes = logical_bytes
            .checked_add(location.length)
            .ok_or_else(|| invalid_error("compaction logical size overflow"))?;
        written_pack_bytes = written_pack_bytes
            .checked_add(location.stored_length)
            .ok_or_else(|| invalid_error("compaction stored size overflow"))?;
        catalog.locations.insert(hash.clone(), location);
    }
    if let Some(pack) = pending {
        finalize_compaction_pack(objects, &mut catalog, &pending_first, pack)?;
    }
    let encoded = encode_index(&catalog)?;
    initialize_format(objects)?;
    publish_index(objects, &encoded)?;
    Ok(CompactionReport {
        retired_packs,
        written_pack_bytes,
        live_objects: live.len() as u64,
        logical_bytes,
    })
}

fn finalize_compaction_pack(
    objects: &Path,
    catalog: &mut Catalog,
    first_hash: &str,
    pack: PendingPack,
) -> Result<()> {
    catalog.packs.insert(pack.name.clone(), pack.length);
    // Input addresses are sorted, so this visits only the current pack, not
    // every earlier pack on each flush.
    for (hash, location) in catalog.locations.range_mut(first_hash.to_owned()..) {
        if location.pack == pack.name {
            location.pack_length = pack.length;
            // Read back this new encoded extent before making it reachable.
            read_range(platform::open_regular(&pack.temporary.0)?, hash, location)?;
        }
    }
    publish_pack(objects, pack)
}

fn new_pending(objects: &Path) -> Result<PendingPack> {
    let id = unique_id();
    let temporary = objects.join(format!(".pack-tmp-{id}"));
    let file = platform::create_new(&temporary, 0o600)?;
    Ok(PendingPack {
        file,
        temporary: Temporary(temporary),
        name: format!("pack-{id}"),
        length: 0,
    })
}

fn append_encoded(
    pending: &mut PendingPack,
    length: u64,
    codec: Codec,
    encoded: &[u8],
) -> Result<Location> {
    let offset = pending.length;
    let stored_length = encoded.len() as u64;
    let pack_length = offset
        .checked_add(stored_length)
        .ok_or_else(|| invalid_error("pack payload length overflow"))?;
    validate_descriptor(offset, length, stored_length, codec, pack_length)?;
    pending.file.write_all(encoded)?;
    pending.length = pack_length;
    Ok(Location {
        pack: pending.name.clone(),
        offset,
        length,
        stored_length,
        codec,
        pack_length,
    })
}

fn publish_pack(objects: &Path, pending: PendingPack) -> Result<()> {
    platform::protect_object(&pending.file)?;
    pending.file.sync_all()?;
    #[cfg(test)]
    fail_finish(FinishPhase::PayloadSynced)?;
    drop(pending.file);
    // NOREPLACE: never replace an immutable file, even on a name collision.
    fs::hard_link(&pending.temporary.0, objects.join(&pending.name))?;
    fs::remove_file(&pending.temporary.0)?;
    platform::sync_directory(objects)?;
    #[cfg(test)]
    fail_finish(FinishPhase::PayloadPublished)?;
    Ok(())
}

fn publish_index(objects: &Path, encoded: &[u8]) -> Result<()> {
    let temporary = objects.join(format!(".pack-index-tmp-{}", unique_id()));
    let mut file = platform::create_new(&temporary, 0o600)?;
    let _cleanup = Temporary(temporary.clone());
    file.write_all(encoded)?;
    platform::protect_object(&file)?;
    file.sync_all()?;
    drop(file);
    #[cfg(test)]
    fail_finish(FinishPhase::BeforeIndexRename)?;
    platform::replace_immutable_file(&temporary, &objects.join(INDEX_NAME)).map_err(|error| {
        io::Error::other(format!(
            "pack index publication failed; outcome may be unknown: {error}"
        ))
    })?;
    let final_barrier = || -> Result<()> {
        #[cfg(test)]
        fail_finish(FinishPhase::IndexRenamed)?;
        platform::sync_directory(objects)?;
        Ok(())
    };
    final_barrier().map_err(|error| {
        io::Error::other(format!("pack index publication outcome unknown: {error}"))
    })?;
    Ok(())
}

/// Exactly one new chunk needs no pack or index rewrite. Use the same synced
/// immutable inode and NOREPLACE publication contract as legacy loose writes.
fn publish_loose(objects: &Path, pending: PendingPack, hash: &str) -> Result<()> {
    platform::protect_object(&pending.file)?;
    pending.file.sync_all()?;
    #[cfg(test)]
    fail_finish(FinishPhase::PayloadSynced)?;
    drop(pending.file);
    let destination = objects.join(hash);
    match fs::hard_link(&pending.temporary.0, &destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = read_loose(platform::open_regular(&destination)?, hash)?;
            let unpublished = read_loose(platform::open_regular(&pending.temporary.0)?, hash)?;
            if existing != unpublished {
                return invalid("concurrent loose object differs from its content address");
            }
            platform::sync_file(&destination)?;
        }
        Err(error) => return Err(error.into()),
    }
    let final_barrier = || -> Result<()> {
        fs::remove_file(&pending.temporary.0)?;
        platform::sync_directory(objects)?;
        #[cfg(test)]
        fail_finish(FinishPhase::PayloadPublished)?;
        Ok(())
    };
    final_barrier().map_err(|error| {
        io::Error::other(format!("loose object publication outcome unknown: {error}"))
    })?;
    Ok(())
}

fn read_format(open: &mut impl FnMut(&str) -> io::Result<File>) -> Result<bool> {
    let file = match open(FORMAT_NAME) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let metadata = platform::file_metadata(&file)?;
    if !metadata.is_file() || metadata.len() != FORMAT_BYTES.len() as u64 {
        return invalid("invalid packed object format marker");
    }
    let mut bytes = Vec::with_capacity(FORMAT_BYTES.len());
    file.take(FORMAT_BYTES.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes != FORMAT_BYTES {
        return invalid("unsupported packed object format marker");
    }
    Ok(true)
}

/// Publish and sync an empty index before the format marker, and both before
/// the first payload. A crash during initialization is restartable; after the
/// marker is durable, a missing index is corruption, never a loose-only store.
fn initialize_format(objects: &Path) -> Result<()> {
    let mut open = |name: &str| platform::open_regular(&objects.join(name));
    if read_format(&mut open)? {
        return Ok(());
    }
    match open(INDEX_NAME) {
        Ok(file) => {
            // Snapshot::load_with already validated this empty, markerless
            // index under our writer lock. Never replace a historical index.
            if platform::file_metadata(&file)?.len() != (HEADER_BYTES + 8) as u64 {
                return invalid("cannot initialize a nonempty markerless pack index");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let temporary = objects.join(format!(".pack-index-tmp-{}", unique_id()));
            let mut file = platform::create_new(&temporary, 0o600)?;
            let _cleanup = Temporary(temporary.clone());
            file.write_all(&encode_index(&Catalog::default())?)?;
            platform::protect_object(&file)?;
            file.sync_all()?;
            drop(file);
            fs::hard_link(&temporary, objects.join(INDEX_NAME))?;
            fs::remove_file(&temporary)?;
        }
        Err(error) => return Err(error.into()),
    }
    platform::sync_directory(objects)?;
    let temporary = objects.join(format!(".pack-format-tmp-{}", unique_id()));
    let mut file = platform::create_new(&temporary, 0o600)?;
    let _cleanup = Temporary(temporary.clone());
    file.write_all(FORMAT_BYTES)?;
    platform::protect_object(&file)?;
    file.sync_all()?;
    drop(file);
    fs::hard_link(&temporary, objects.join(FORMAT_NAME))?;
    fs::remove_file(&temporary)?;
    platform::sync_directory(objects)?;
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinishPhase {
    PayloadSynced,
    PayloadPublished,
    BeforeIndexRename,
    IndexRenamed,
}

#[cfg(test)]
thread_local! {
    static FINISH_FAULT: std::cell::Cell<Option<FinishPhase>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_failed_finish<T>(operation: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FINISH_FAULT.with(|fault| fault.set(None));
        }
    }
    FINISH_FAULT.with(|fault| fault.set(Some(FinishPhase::PayloadSynced)));
    let _reset = Reset;
    operation()
}

#[cfg(test)]
fn fail_finish(phase: FinishPhase) -> Result<()> {
    if FINISH_FAULT.with(|fault| fault.get() == Some(phase)) {
        return Err(io::Error::other(format!("injected pack finish failure at {phase:?}")).into());
    }
    Ok(())
}

fn read_loose(file: File, hash: &str) -> Result<Vec<u8>> {
    let metadata = platform::file_metadata(&file)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CHUNK_BYTES {
        return invalid(format!("invalid loose chunk length for {hash}"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CHUNK_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() || blake3::hash(&bytes).to_hex().as_str() != hash {
        return invalid(format!("corrupt object: {hash}"));
    }
    Ok(bytes)
}

fn read_range(mut file: File, hash: &str, location: &Location) -> Result<Vec<u8>> {
    validate_location(location)?;
    let metadata = platform::file_metadata(&file)?;
    if !metadata.is_file() || metadata.len() != location.pack_length {
        return invalid(format!("pack payload length mismatch: {}", location.pack));
    }
    file.seek(SeekFrom::Start(location.offset))?;
    let mut stored = vec![0; location.stored_length as usize];
    file.read_exact(&mut stored)?;
    let bytes = decode_payload(&stored, location.codec, location.length)?;
    if blake3::hash(&bytes).to_hex().as_str() != hash {
        return invalid(format!("corrupt packed object: {hash}"));
    }
    Ok(bytes)
}

/// Reads the immutable writer policy once per batch/compaction. Standalone
/// object directories without a policy retain selective compression by default.
/// Readers never consult this file: each indexed extent declares its own codec.
pub(crate) fn compression_enabled(objects: &Path) -> Result<bool> {
    validate_directory_chain(objects)?;
    let file = match platform::open_regular(&objects.join(COMPRESSION_NAME)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let length = platform::file_metadata(&file)?.len();
    if !(5..=6).contains(&length) {
        return invalid("invalid object compression policy length");
    }
    let mut bytes = Vec::with_capacity(6);
    file.take(7).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        return invalid("object compression policy changed length while reading");
    }
    match bytes.as_slice() {
        b"auto\n" => Ok(true),
        b"never\n" => Ok(false),
        _ => invalid("unsupported object compression policy; expected auto or never"),
    }
}

#[cfg(test)]
fn encode_payload(bytes: &[u8]) -> Result<(Codec, Vec<u8>)> {
    encode_with_policy(bytes, true)
}

fn encode_with_policy(bytes: &[u8], compression: bool) -> Result<(Codec, Vec<u8>)> {
    // Tiny and already-compressed objects must not pay codec framing overhead.
    if compression && bytes.len() >= 128 {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1)?;
        encoder.window_log(CODEC_WINDOW_LOG)?;
        encoder.include_checksum(true)?;
        encoder.set_pledged_src_size(Some(bytes.len() as u64))?;
        encoder.write_all(bytes)?;
        let encoded = encoder.finish()?;
        // Require both a useful absolute saving and at least 1/16 saving.
        if encoded.len() + 32 <= bytes.len() && encoded.len() * 16 <= bytes.len() * 15 {
            return Ok((Codec::Zstd, encoded));
        }
    }
    Ok((Codec::Raw, bytes.to_vec()))
}

fn decode_payload(stored: &[u8], codec: Codec, length: u64) -> Result<Vec<u8>> {
    if length == 0
        || length > MAX_CHUNK_BYTES
        || stored.is_empty()
        || stored.len() as u64 > MAX_CHUNK_BYTES
    {
        return invalid("invalid packed codec input or output length");
    }
    match codec {
        Codec::Raw => {
            if stored.len() as u64 != length {
                return invalid("raw packed object length mismatch");
            }
            Ok(stored.to_vec())
        }
        Codec::Zstd => {
            if stored.len() < 5 || stored[..4] != [0x28, 0xb5, 0x2f, 0xfd] || stored[4] & 0x04 == 0
            {
                return invalid("packed object requires a checksummed zstd frame");
            }
            let mut decoder = zstd::stream::read::Decoder::with_buffer(stored)?.single_frame();
            decoder.window_log_max(CODEC_WINDOW_LOG)?;
            let mut bytes = Vec::with_capacity(length as usize);
            decoder.by_ref().take(length + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 != length {
                return invalid("decoded packed object length mismatch");
            }
            if !decoder.finish().is_empty() {
                return invalid("trailing data or frames after packed object");
            }
            Ok(bytes)
        }
    }
}

fn validate_location(location: &Location) -> Result<()> {
    validate_descriptor(
        location.offset,
        location.length,
        location.stored_length,
        location.codec,
        location.pack_length,
    )?;
    validate_pack_name(&location.pack)
}

fn validate_descriptor(
    offset: u64,
    length: u64,
    stored_length: u64,
    codec: Codec,
    pack_length: u64,
) -> Result<()> {
    if length == 0
        || length > MAX_CHUNK_BYTES
        || stored_length == 0
        || stored_length > MAX_CHUNK_BYTES
        || pack_length == 0
        || (codec == Codec::Raw && length != stored_length)
        || (codec == Codec::Zstd && stored_length >= length)
        || offset
            .checked_add(stored_length)
            .is_none_or(|end| end > pack_length)
    {
        return invalid("packed object has invalid codec lengths or out-of-bounds offset");
    }
    Ok(())
}

fn index_size(packs: usize, objects: usize) -> Result<usize> {
    let length = packs
        .checked_mul(PACK_RECORD_BYTES)
        .and_then(|size| objects.checked_mul(OBJECT_RECORD_BYTES)?.checked_add(size))
        .and_then(|size| size.checked_add(HEADER_BYTES + 8))
        .ok_or_else(|| invalid_error("pack index size overflow"))?;
    if length > MAX_INDEX_BYTES || packs > u32::MAX as usize || objects > u32::MAX as usize {
        return invalid("pack index exceeds the supported 64 MiB limit");
    }
    Ok(length)
}

fn encode_index(snapshot: &Catalog) -> Result<Vec<u8>> {
    let length = index_size(snapshot.packs.len(), snapshot.locations.len())?;
    let mut bytes = Vec::with_capacity(length);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&((length - HEADER_BYTES) as u64).to_le_bytes());
    bytes.extend_from_slice(&[0; 32]);
    bytes.extend_from_slice(&(snapshot.packs.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(snapshot.locations.len() as u32).to_le_bytes());
    let mut pack_numbers = BTreeMap::new();
    for (number, (name, length)) in snapshot.packs.iter().enumerate() {
        validate_pack_name(name)?;
        if *length == 0 {
            return invalid("indexed pack must not be empty");
        }
        pack_numbers.insert(name.as_str(), number as u32);
        bytes.extend_from_slice(&hash_bytes(&name[5..])?);
        bytes.extend_from_slice(&length.to_le_bytes());
    }
    for (hash, location) in &snapshot.locations {
        validate_location(location)?;
        if snapshot.packs.get(&location.pack) != Some(&location.pack_length) {
            return invalid("packed object disagrees with its pack length");
        }
        let number = pack_numbers
            .get(location.pack.as_str())
            .ok_or_else(|| invalid_error("object refers to an unknown pack"))?;
        bytes.extend_from_slice(&hash_bytes(hash)?);
        bytes.extend_from_slice(&number.to_le_bytes());
        bytes.extend_from_slice(&location.offset.to_le_bytes());
        bytes.extend_from_slice(&(location.length as u32).to_le_bytes());
        bytes.extend_from_slice(&(location.stored_length as u32).to_le_bytes());
        bytes.push(location.codec as u8);
    }
    let checksum = blake3::hash(&bytes[HEADER_BYTES..]);
    bytes[20..HEADER_BYTES].copy_from_slice(checksum.as_bytes());
    Ok(bytes)
}

fn decode_index(bytes: Vec<u8>) -> Result<Snapshot> {
    if bytes.len() < HEADER_BYTES + 8 || bytes.len() > MAX_INDEX_BYTES {
        return invalid("invalid pack index length");
    }
    let mut cursor = bytes.as_slice();
    if field::<8>(&mut cursor)? != *MAGIC || u32::from_le_bytes(field(&mut cursor)?) != VERSION {
        return invalid("unsupported pack index format or version");
    }
    let body_length = u64::from_le_bytes(field(&mut cursor)?);
    let checksum = field::<32>(&mut cursor)?;
    if body_length != cursor.len() as u64 || blake3::hash(cursor).as_bytes() != &checksum {
        return invalid("corrupt pack index length or checksum");
    }
    let pack_count = u32::from_le_bytes(field(&mut cursor)?) as usize;
    let object_count = u32::from_le_bytes(field(&mut cursor)?) as usize;
    if index_size(pack_count, object_count)? != bytes.len() {
        return invalid("pack index record counts do not match its length");
    }
    // The exact-size check above proves every fixed-width record is present.
    // Validate in place with stack-sized fields, without per-entry allocation.
    let (pack_bytes, object_bytes) = cursor.split_at(pack_count * PACK_RECORD_BYTES);
    let packs = pack_bytes.as_chunks::<PACK_RECORD_BYTES>().0;
    let mut previous_pack = None;
    for pack in packs {
        let id: [u8; 32] = pack[..32].try_into().expect("fixed field");
        let length = u64::from_le_bytes(pack[32..40].try_into().expect("fixed field"));
        if length == 0 || previous_pack.is_some_and(|previous| previous >= id) {
            return invalid("pack index contains empty, duplicate or unsorted packs");
        }
        previous_pack = Some(id);
    }
    let mut previous_hash = None;
    for record in object_bytes.as_chunks::<OBJECT_RECORD_BYTES>().0 {
        let hash: [u8; 32] = record[..32].try_into().expect("fixed field");
        if previous_hash.is_some_and(|previous| previous >= hash) {
            return invalid("pack index contains duplicate or unsorted object addresses");
        }
        previous_hash = Some(hash);
        let number = u32::from_le_bytes(record[32..36].try_into().expect("fixed field")) as usize;
        let offset = u64::from_le_bytes(record[36..44].try_into().expect("fixed field"));
        let length = u32::from_le_bytes(record[44..48].try_into().expect("fixed field")) as u64;
        let pack = packs
            .get(number)
            .ok_or_else(|| invalid_error("object refers to an unknown pack"))?;
        let pack_length = u64::from_le_bytes(pack[32..40].try_into().expect("fixed field"));
        let stored_length =
            u32::from_le_bytes(record[48..52].try_into().expect("fixed field")) as u64;
        let codec = Codec::decode(record[52])?;
        validate_descriptor(offset, length, stored_length, codec, pack_length)?;
    }
    Ok(Snapshot {
        bytes,
        pack_count,
        object_count,
    })
}

fn field<const N: usize>(cursor: &mut &[u8]) -> Result<[u8; N]> {
    if cursor.len() < N {
        return invalid("truncated pack index");
    }
    let (value, rest) = cursor.split_at(N);
    *cursor = rest;
    Ok(value.try_into().expect("field length checked"))
}

fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid("chunk address must be 64 lowercase hexadecimal bytes");
    }
    Ok(())
}

fn validate_pack_name(name: &str) -> Result<()> {
    let Some(id) = name.strip_prefix("pack-") else {
        return invalid("invalid pack filename");
    };
    validate_hash(id)
}

fn hash_bytes(hash: &str) -> Result<[u8; 32]> {
    validate_hash(hash)?;
    let digit = |byte: u8| {
        if byte.is_ascii_digit() {
            byte - b'0'
        } else {
            byte - b'a' + 10
        }
    };
    let mut output = [0; 32];
    for (index, pair) in hash.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        output[index] = digit(pair[0]) << 4 | digit(pair[1]);
    }
    Ok(output)
}

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for &byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn validate_directory_chain(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return invalid("empty object directory path");
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return invalid("parent-directory syntax is forbidden"),
            Component::CurDir => {
                if current.as_os_str().is_empty() {
                    current.push(".");
                }
            }
            Component::Prefix(_) => {
                current.push(component.as_os_str());
                continue;
            }
            other => current.push(other.as_os_str()),
        }
        let metadata = platform::symlink_metadata(&current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return invalid("object directory path contains a symlink or non-directory");
        }
    }
    Ok(())
}

fn unique_id() -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sprig-immutable-pack-v1");
    hash.update(&std::process::id().to_le_bytes());
    hash.update(&NONCE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hash.update(
        &SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    hash.finalize().to_hex().to_string()
}

fn invalid_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(invalid_error(message).into())
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        // Only remove this writer's unique temporary name, never a published
        // payload, the index, or another writer's persistent lock.
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("sprig-packs-{}", unique_id()));
            fs::create_dir(&path).unwrap();
            // macOS's temporary-directory prefix can contain system symlinks.
            // Resolve our synthetic fixture once; object paths still reject links.
            Self(fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn overwrite(path: &Path, bytes: &[u8]) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn compressed_payloads_keep_plain_addresses_and_bound_decoding() {
        let fixture = Fixture::new();
        assert!(
            compression_enabled(&fixture.0).unwrap(),
            "standalone default is auto"
        );
        let compressible = vec![b'x'; MAX_CHUNK_BYTES as usize];
        for (policy, enabled) in [(b"auto\n".as_slice(), true), (b"never\n".as_slice(), false)] {
            let configured = Fixture::new();
            let policy_path = configured.0.join(COMPRESSION_NAME);
            fs::write(&policy_path, policy).unwrap();
            assert_eq!(compression_enabled(&configured.0).unwrap(), enabled);
            let mut writer = Batch::open(&configured.0).unwrap();
            // Synthetic tampering only: production policy has no mutation API.
            // An open writer pins its policy, while readers need no policy I/O.
            fs::write(&policy_path, b"unknown policy").unwrap();
            let address = writer.put(&compressible).unwrap();
            writer.finish().unwrap();
            assert_eq!(
                Reader::open(&configured.0).unwrap().read(&address).unwrap(),
                compressible
            );
            assert_eq!(
                PointReader::open(&configured.0)
                    .unwrap()
                    .read(&address)
                    .unwrap(),
                compressible
            );
            let written = Reader::open(&configured.0).unwrap().locations();
            if enabled {
                assert_eq!(written[&address].codec, Codec::Zstd);
            } else {
                assert!(!written.contains_key(&address));
                assert_eq!(fs::read(configured.0.join(&address)).unwrap(), compressible);
            }
            assert!(Batch::open(&configured.0).is_err());
            let live = BTreeSet::from([address.clone()]);
            assert!(compact(&configured.0, &live).is_err());
            fs::write(&policy_path, policy).unwrap();
            compact(&configured.0, &live).unwrap();
            let location = Reader::open(&configured.0).unwrap().locations()[&address].clone();
            assert_eq!(
                location.codec,
                if enabled { Codec::Zstd } else { Codec::Raw }
            );
            assert_eq!(location.length, MAX_CHUNK_BYTES);
            if !enabled {
                assert_eq!(location.stored_length, MAX_CHUNK_BYTES);
            }
        }
        let invalid_policy = Fixture::new();
        for bytes in [
            b"".as_slice(),
            b"auto",
            b"AUTO\n",
            b"never\nextra",
            b"other\n",
        ] {
            fs::write(invalid_policy.0.join(COMPRESSION_NAME), bytes).unwrap();
            assert!(compression_enabled(&invalid_policy.0).is_err());
        }
        #[cfg(unix)]
        {
            let linked = Fixture::new();
            fs::write(invalid_policy.0.join(COMPRESSION_NAME), b"auto\n").unwrap();
            std::os::unix::fs::symlink(
                invalid_policy.0.join(COMPRESSION_NAME),
                linked.0.join(COMPRESSION_NAME),
            )
            .unwrap();
            assert!(compression_enabled(&linked.0).is_err());
        }
        let expected_hash = blake3::hash(&compressible).to_hex().to_string();
        let mut batch = Batch::open(&fixture.0).unwrap();
        assert_eq!(batch.put(&compressible).unwrap(), expected_hash);
        assert_eq!(batch.put(&compressible).unwrap(), expected_hash);
        batch.finish().unwrap();
        assert!(
            !fixture.0.join(&expected_hash).exists(),
            "compressed singleton is indexed"
        );
        let reader = Reader::open(&fixture.0).unwrap();
        let location = reader.locations()[&expected_hash].clone();
        assert_eq!(location.codec, Codec::Zstd);
        assert_eq!(location.length, MAX_CHUNK_BYTES);
        assert!(location.stored_length < 1024);
        assert_eq!(reader.read(&expected_hash).unwrap(), compressible);
        assert_eq!(reader.length(&expected_hash).unwrap(), MAX_CHUNK_BYTES);
        assert_eq!(
            PointReader::open(&fixture.0)
                .unwrap()
                .read(&expected_hash)
                .unwrap(),
            compressible
        );
        assert_eq!(reader.object_sizes()[&expected_hash], MAX_CHUNK_BYTES);

        let mut random = vec![0; MAX_CHUNK_BYTES as usize];
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"deterministic incompressible packed test bytes");
        hasher.finalize_xof().fill(&mut random);
        let (codec, encoded) = encode_payload(&random).unwrap();
        assert_eq!(codec, Codec::Raw);
        assert_eq!(encoded, random);
        let mut mixed = Batch::open(&fixture.0).unwrap();
        let random_hash = mixed.put(&random).unwrap();
        mixed.put(b"raw companion").unwrap();
        mixed.finish().unwrap();
        let mixed_reader = Reader::open(&fixture.0).unwrap();
        let raw_location = mixed_reader.locations()[&random_hash].clone();
        assert_eq!(raw_location.codec, Codec::Raw);
        assert_eq!(raw_location.stored_length, MAX_CHUNK_BYTES);
        assert_eq!(mixed_reader.read(&random_hash).unwrap(), random);
        let mut dedup = Batch::open(&fixture.0).unwrap();
        assert_eq!(dedup.put(&compressible).unwrap(), expected_hash);
        dedup.finish().unwrap();

        let (codec, encoded) = encode_payload(&compressible).unwrap();
        assert_eq!(
            decode_payload(&encoded, codec, MAX_CHUNK_BYTES).unwrap(),
            compressible
        );
        assert!(decode_payload(&encoded, codec, MAX_CHUNK_BYTES - 1).is_err());
        assert!(decode_payload(&encoded, codec, MAX_CHUNK_BYTES + 1).is_err());
        assert!(decode_payload(&encoded[..encoded.len() - 1], codec, MAX_CHUNK_BYTES).is_err());
        let mut concatenated = encoded.clone();
        concatenated.extend_from_slice(&encoded);
        assert!(decode_payload(&concatenated, codec, MAX_CHUNK_BYTES).is_err());
        let mut corrupt = encoded.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(decode_payload(&corrupt, codec, MAX_CHUNK_BYTES).is_err());
        let (_, bomb) = encode_payload(&vec![b'b'; MAX_CHUNK_BYTES as usize * 2]).unwrap();
        assert!(decode_payload(&bomb, Codec::Zstd, MAX_CHUNK_BYTES).is_err());
        assert!(validate_descriptor(0, 3, 4, Codec::Raw, 4).is_err());
        assert!(validate_descriptor(u64::MAX, 4, 3, Codec::Zstd, u64::MAX).is_err());

        // A requested descriptor must validate codec and both lengths, while
        // unrelated descriptors remain the full Reader's audit responsibility.
        let index_path = fixture.0.join(INDEX_NAME);
        let original = fs::read(&index_path).unwrap();
        let snapshot = decode_index(original.clone()).unwrap();
        let number = snapshot
            .object_records()
            .iter()
            .position(|record| record[..32] == hash_bytes(&expected_hash).unwrap())
            .unwrap();
        let start = HEADER_BYTES
            + 8
            + snapshot.pack_count * PACK_RECORD_BYTES
            + number * OBJECT_RECORD_BYTES;
        for (offset, replacement) in [
            (52, vec![255]),
            (44, ((MAX_CHUNK_BYTES + 1) as u32).to_le_bytes().to_vec()),
            (48, 0_u32.to_le_bytes().to_vec()),
        ] {
            let mut bad = original.clone();
            bad[start + offset..start + offset + replacement.len()].copy_from_slice(&replacement);
            let checksum = blake3::hash(&bad[HEADER_BYTES..]);
            bad[20..HEADER_BYTES].copy_from_slice(checksum.as_bytes());
            overwrite(&index_path, &bad);
            assert!(Reader::open(&fixture.0).is_err());
            assert!(
                PointReader::open(&fixture.0)
                    .unwrap()
                    .read(&expected_hash)
                    .is_err()
            );
        }
        overwrite(&index_path, &original);
    }

    #[test]
    fn compaction_keeps_reader_generations_and_reclaims_only_under_both_locks() {
        let fixture = Fixture::new();
        let mut batch = Batch::open(&fixture.0).unwrap();
        let a = batch.put(&vec![b'a'; 16 * 1024]).unwrap();
        let b = batch.put(b"small raw live object").unwrap();
        let dead = batch.put(b"unreachable packed object").unwrap();
        batch.finish().unwrap();
        let loose_bytes = b"live loose object";
        let loose = blake3::hash(loose_bytes).to_hex().to_string();
        fs::write(fixture.0.join(&loose), loose_bytes).unwrap();
        let dead_loose_bytes = b"unreachable loose object";
        let dead_loose = blake3::hash(dead_loose_bytes).to_hex().to_string();
        fs::write(fixture.0.join(&dead_loose), dead_loose_bytes).unwrap();
        let live = BTreeSet::from([a.clone(), b.clone(), loose.clone()]);
        let old = Reader::open(&fixture.0).unwrap();
        let mut old_point = PointReader::open(&fixture.0).unwrap();
        let old_packs = old.pack_sizes();
        assert!(try_exclusive_readers(&fixture.0).unwrap().is_none());
        let report = compact_with_limit(&fixture.0, &live, 32).unwrap();
        assert_eq!(report.retired_packs, old_packs);
        assert_eq!(report.live_objects, 3);
        assert_eq!(
            report.logical_bytes,
            16 * 1024 + b"small raw live object".len() as u64 + loose_bytes.len() as u64
        );
        assert!(report.written_pack_bytes < report.logical_bytes);
        for pack in old_packs.keys() {
            assert!(fixture.0.join(pack).exists());
        }
        assert_eq!(old.read(&dead).unwrap(), b"unreachable packed object");
        assert_eq!(old_point.read(&dead).unwrap(), b"unreachable packed object");
        let new = Reader::open(&fixture.0).unwrap();
        assert_eq!(new.pack_sizes().len(), 3, "small test cap rotates packs");
        assert_eq!(
            new.object_sizes().into_keys().collect::<BTreeSet<_>>(),
            live
        );
        assert!(new.read(&dead).is_err());
        assert_eq!(new.read(&loose).unwrap(), loose_bytes);
        assert!(
            fixture.0.join(&loose).exists(),
            "compaction alone deletes no loose aliases"
        );
        let busy = reclaim_retired(&fixture.0, &live).unwrap();
        assert!(busy.readers_busy);
        assert_eq!(busy.removed_files, 0);
        drop(old);
        drop(old_point);
        drop(new);

        let guard = try_exclusive_readers(&fixture.0).unwrap().unwrap();
        assert!(Reader::open(&fixture.0).is_err());
        assert!(PointReader::open(&fixture.0).is_err());
        drop(guard);
        assert!(reclaim_retired(&fixture.0, &BTreeSet::from([a.clone()])).is_err());
        for pack in old_packs.keys() {
            assert!(fixture.0.join(pack).exists());
        }
        fs::write(fixture.0.join(".pack-tmp-unrecognized"), b"keep").unwrap();
        fs::write(fixture.0.join("pack-not-an-address"), b"keep").unwrap();
        fs::create_dir(fixture.0.join("0".repeat(64))).unwrap();
        let reclaimed = reclaim_retired(&fixture.0, &live).unwrap();
        assert!(!reclaimed.readers_busy);
        assert_eq!(reclaimed.removed_files, old_packs.len() as u64 + 2);
        assert_eq!(
            reclaimed.removed_bytes,
            old_packs.values().sum::<u64>()
                + loose_bytes.len() as u64
                + dead_loose_bytes.len() as u64
        );
        for pack in old_packs.keys() {
            assert!(!fixture.0.join(pack).exists());
        }
        assert!(fixture.0.join(".pack-tmp-unrecognized").exists());
        assert!(fixture.0.join("pack-not-an-address").exists());
        assert!(fixture.0.join("0".repeat(64)).is_dir());
        assert_eq!(
            Reader::open(&fixture.0).unwrap().read(&loose).unwrap(),
            loose_bytes
        );
        assert_eq!(reclaim_retired(&fixture.0, &live).unwrap().removed_files, 0);

        // A new writer invalidates an old compaction live-set plan.
        let mut later = Batch::open(&fixture.0).unwrap();
        later.put(b"later packed object").unwrap();
        later.put(b"later packed companion").unwrap();
        later.finish().unwrap();
        assert!(reclaim_retired(&fixture.0, &live).is_err());

        // Reclamation audits the retained payload before deleting any backup.
        let damaged = Fixture::new();
        let mut seed = Batch::open(&damaged.0).unwrap();
        let hash = seed.put(&vec![b'z'; 4096]).unwrap();
        seed.finish().unwrap();
        let live = BTreeSet::from([hash.clone()]);
        let report = compact(&damaged.0, &live).unwrap();
        let current = Reader::open(&damaged.0).unwrap().locations()[&hash].clone();
        let path = damaged.0.join(current.pack);
        let mut payload = fs::read(&path).unwrap();
        payload[0] ^= 1;
        overwrite(&path, &payload);
        assert!(reclaim_retired(&damaged.0, &live).is_err());
        for pack in report.retired_packs.keys() {
            assert!(damaged.0.join(pack).exists());
        }
    }

    #[test]
    fn compaction_failpoints_preserve_old_payload_and_published_index_outcomes() {
        for phase in [
            FinishPhase::PayloadSynced,
            FinishPhase::PayloadPublished,
            FinishPhase::BeforeIndexRename,
            FinishPhase::IndexRenamed,
        ] {
            let fixture = Fixture::new();
            let mut batch = Batch::open(&fixture.0).unwrap();
            let keep = batch.put(&vec![b'k'; 4096]).unwrap();
            let dead = batch.put(b"old reachable only by prior reader").unwrap();
            batch.finish().unwrap();
            let old = Reader::open(&fixture.0).unwrap();
            let old_packs = old.pack_sizes();
            let live = BTreeSet::from([keep.clone()]);
            FINISH_FAULT.with(|fault| fault.set(Some(phase)));
            let error = compact(&fixture.0, &live).unwrap_err().to_string();
            FINISH_FAULT.with(|fault| fault.set(None));
            for pack in old_packs.keys() {
                assert!(fixture.0.join(pack).exists());
            }
            assert_eq!(
                old.read(&dead).unwrap(),
                b"old reachable only by prior reader"
            );
            let current = Reader::open(&fixture.0).unwrap();
            assert_eq!(current.read(&keep).unwrap(), vec![b'k'; 4096]);
            if phase == FinishPhase::IndexRenamed {
                assert!(error.contains("outcome unknown"));
                assert!(current.read(&dead).is_err());
            } else {
                assert!(current.read(&dead).is_ok());
            }
            drop(current);
            drop(old);
            compact(&fixture.0, &live).unwrap();
            reclaim_retired(&fixture.0, &live).unwrap();
            assert_eq!(
                Reader::open(&fixture.0).unwrap().read(&keep).unwrap(),
                vec![b'k'; 4096]
            );
        }
        let empty = Fixture::new();
        assert_eq!(compact(&empty.0, &BTreeSet::new()).unwrap().live_objects, 0);
        assert_eq!(
            reclaim_retired(&empty.0, &BTreeSet::new())
                .unwrap()
                .removed_files,
            0
        );
    }

    #[test]
    fn packed_batches_preserve_snapshots_verify_dedup_and_reject_damage() {
        // Exercise every binary-search position, gaps and catalog expansion.
        let name = format!("pack-{}", "f".repeat(64));
        let mut catalog = Catalog::default();
        catalog.packs.insert(name.clone(), 256);
        for index in 0..256_u64 {
            catalog.locations.insert(
                format!("{:064x}", index * 2 + 1),
                Location {
                    pack: name.clone(),
                    offset: index,
                    length: 1,
                    stored_length: 1,
                    codec: Codec::Raw,
                    pack_length: 256,
                },
            );
        }
        let encoded = encode_index(&catalog).unwrap();
        let compact = decode_index(encoded.clone()).unwrap();
        assert_eq!(compact.bytes, encoded);
        assert_eq!(compact.object_count, 256);
        assert_eq!(compact.locations(), catalog.locations);
        assert_eq!(compact.pack_sizes(), catalog.packs);
        for (hash, location) in &catalog.locations {
            assert_eq!(compact.location(hash).unwrap().as_ref(), Some(location));
        }
        for absent in [0_u64, 2, 512] {
            assert!(
                compact
                    .location(&format!("{absent:064x}"))
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(
            encode_index(&Catalog {
                locations: compact.locations(),
                packs: compact.pack_sizes(),
            })
            .unwrap(),
            encoded
        );
        let mut duplicate = encoded.clone();
        let object_start = HEADER_BYTES + 8 + PACK_RECORD_BYTES;
        duplicate.copy_within(
            object_start..object_start + 32,
            object_start + OBJECT_RECORD_BYTES,
        );
        let checksum = blake3::hash(&duplicate[HEADER_BYTES..]);
        duplicate[20..HEADER_BYTES].copy_from_slice(checksum.as_bytes());
        assert!(
            decode_index(duplicate).is_err(),
            "raw sortedness validation rejects duplicate hashes"
        );

        // Point reads promise only requested-content integrity. Auditing the
        // entire index remains the full reader's separate responsibility.
        let point_fixture = Fixture::new();
        let mut point_batch = Batch::open(&point_fixture.0).unwrap();
        let mut expected = BTreeMap::new();
        for bytes in [b"point-a", b"point-b", b"point-c", b"point-d"] {
            expected.insert(point_batch.put(bytes).unwrap(), bytes.to_vec());
        }
        point_batch.finish().unwrap();
        let mut point = PointReader::open(&point_fixture.0).unwrap();
        for (hash, bytes) in &expected {
            assert_eq!(point.read(hash).unwrap(), *bytes);
        }
        assert!(point.read("../pack-index").is_err());
        let point_index_path = point_fixture.0.join(INDEX_NAME);
        let point_index = fs::read(&point_index_path).unwrap();
        let point_locations = Reader::open(&point_fixture.0).unwrap().locations();
        let (selected_hash, selected_bytes) = expected.iter().nth(expected.len() / 2).unwrap();
        let selected = &point_locations[selected_hash];
        let object_start = HEADER_BYTES + 8 + PACK_RECORD_BYTES;
        let selected_start = object_start + (expected.len() / 2) * OBJECT_RECORD_BYTES;

        let mut damaged = point_index.clone();
        damaged[HEADER_BYTES + 4..HEADER_BYTES + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        overwrite(&point_index_path, &damaged);
        assert!(
            PointReader::open(&point_fixture.0).is_err(),
            "malformed counts are rejected at open"
        );

        overwrite(&point_index_path, &point_index);
        let mut truncated = PointReader::open(&point_fixture.0).unwrap();
        overwrite(&point_index_path, &point_index[..point_index.len() - 1]);
        assert!(PointReader::open(&point_fixture.0).is_err());
        assert!(truncated.read(selected_hash).is_err());
        overwrite(&point_index_path, &point_index);
        #[cfg(unix)]
        {
            let real_index = point_fixture.0.join("saved-index");
            fs::rename(&point_index_path, &real_index).unwrap();
            std::os::unix::fs::symlink(&real_index, &point_index_path).unwrap();
            assert!(PointReader::open(&point_fixture.0).is_err());
            fs::remove_file(&point_index_path).unwrap();
            fs::rename(real_index, &point_index_path).unwrap();
        }

        let other = point_locations
            .iter()
            .find(|(hash, _)| *hash != selected_hash)
            .unwrap()
            .1;
        damaged = point_index.clone();
        damaged[selected_start + 36..selected_start + 44]
            .copy_from_slice(&other.offset.to_le_bytes());
        overwrite(&point_index_path, &damaged);
        assert!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .is_err(),
            "valid but wrong offsets cannot return another object's bytes"
        );
        damaged[selected_start + 36..selected_start + 44].copy_from_slice(&u64::MAX.to_le_bytes());
        overwrite(&point_index_path, &damaged);
        assert!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .is_err(),
            "selected extent overflow is rejected"
        );
        damaged = point_index.clone();
        damaged[selected_start + 32..selected_start + 36].copy_from_slice(&u32::MAX.to_le_bytes());
        overwrite(&point_index_path, &damaged);
        assert!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .is_err(),
            "selected pack references are bounded"
        );

        damaged = point_index.clone();
        damaged[object_start + 36..object_start + 44].copy_from_slice(&u64::MAX.to_le_bytes());
        let checksum = blake3::hash(&damaged[HEADER_BYTES..]);
        damaged[20..HEADER_BYTES].copy_from_slice(checksum.as_bytes());
        overwrite(&point_index_path, &damaged);
        assert!(
            Reader::open(&point_fixture.0).is_err(),
            "full readers reject unvisited bad extents"
        );
        assert_eq!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .unwrap(),
            *selected_bytes
        );
        // A nonmatching midpoint need not resolve its pack/extent. It cannot
        // authorize bytes for the requested hash; full readers still audit it.
        damaged = point_index.clone();
        damaged[selected_start + 32..selected_start + 36].copy_from_slice(&u32::MAX.to_le_bytes());
        let checksum = blake3::hash(&damaged[HEADER_BYTES..]);
        damaged[20..HEADER_BYTES].copy_from_slice(checksum.as_bytes());
        overwrite(&point_index_path, &damaged);
        assert!(Reader::open(&point_fixture.0).is_err());
        let (first_hash, first_bytes) = expected.first_key_value().unwrap();
        assert_eq!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(first_hash)
                .unwrap(),
            *first_bytes
        );
        damaged = point_index.clone();
        damaged[20] ^= 1;
        overwrite(&point_index_path, &damaged);
        assert!(
            Reader::open(&point_fixture.0).is_err(),
            "full readers enforce the global checksum"
        );
        assert_eq!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .unwrap(),
            *selected_bytes
        );

        // Destroy ordering so this lookup misses. Loose fallback is allowed
        // only when its independently verified content has the original hash.
        damaged = point_index.clone();
        damaged[selected_start..selected_start + 32].fill(0);
        overwrite(&point_index_path, &damaged);
        assert!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .is_err()
        );
        fs::write(point_fixture.0.join(selected_hash), b"wrong loose bytes").unwrap();
        assert!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .is_err()
        );
        fs::write(point_fixture.0.join(selected_hash), selected_bytes).unwrap();
        assert_eq!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .unwrap(),
            *selected_bytes
        );
        fs::remove_file(point_fixture.0.join(selected_hash)).unwrap();
        overwrite(&point_index_path, &point_index);

        // A real packed hit must not hide corruption behind a valid loose alias.
        fs::write(point_fixture.0.join(selected_hash), selected_bytes).unwrap();
        let selected_pack = point_fixture.0.join(&selected.pack);
        let original_payload = fs::read(&selected_pack).unwrap();
        let mut bad_payload = original_payload.clone();
        bad_payload[selected.offset as usize] ^= 1;
        overwrite(&selected_pack, &bad_payload);
        assert!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .is_err()
        );
        overwrite(
            &selected_pack,
            &original_payload[..original_payload.len() - 1],
        );
        assert!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(selected_hash)
                .is_err(),
            "actual payload lengths are checked"
        );
        overwrite(&selected_pack, &original_payload);
        fs::remove_file(point_fixture.0.join(selected_hash)).unwrap();

        let mut pinned = PointReader::open(&point_fixture.0).unwrap();
        let mut later = Batch::open(&point_fixture.0).unwrap();
        let later_hash = later.put(b"later point object").unwrap();
        later.put(b"later point companion").unwrap();
        later.finish().unwrap();
        assert_eq!(pinned.read(selected_hash).unwrap(), *selected_bytes);
        assert!(
            pinned.read(&later_hash).is_err(),
            "one reader pins one immutable index generation"
        );
        assert_eq!(
            PointReader::open(&point_fixture.0)
                .unwrap()
                .read(&later_hash)
                .unwrap(),
            b"later point object"
        );

        let fixture = Fixture::new();
        let objects = &fixture.0;
        let loose = b"legacy loose content";
        let loose_hash = blake3::hash(loose).to_hex().to_string();
        fs::write(objects.join(&loose_hash), loose).unwrap();
        let old = Reader::open(objects).unwrap();
        let mut batch = Batch::open(objects).unwrap();
        assert!(Batch::open(objects).is_err(), "writer lock must not block");
        assert_eq!(batch.put(loose).unwrap(), loose_hash);
        let a = batch.put(b"first packed chunk").unwrap();
        assert_eq!(batch.put(b"first packed chunk").unwrap(), a);
        let b = batch.put(b"second packed chunk").unwrap();
        assert!(old.read(&a).is_err());
        batch.finish().unwrap();
        let first = Reader::open(objects).unwrap();
        assert_eq!(first.read(&a).unwrap(), b"first packed chunk");
        assert_eq!(first.length(&b).unwrap(), 19);
        assert_eq!(first.read(&loose_hash).unwrap(), loose);
        assert_eq!(first.locations().len(), 2);
        assert_eq!(first.pack_sizes().len(), 1);
        assert!(!objects.join(&a).exists());
        let snapshot =
            Snapshot::load_with(|name| platform::open_regular(&objects.join(name))).unwrap();
        assert_eq!(
            snapshot
                .read_with(&b, |name| platform::open_regular(&objects.join(name)))
                .unwrap(),
            b"second packed chunk"
        );

        let original_index = fs::read(objects.join(INDEX_NAME)).unwrap();
        let mut dedup = Batch::open(objects).unwrap();
        assert_eq!(dedup.put(b"first packed chunk").unwrap(), a);
        dedup.finish().unwrap();
        assert_eq!(fs::read(objects.join(INDEX_NAME)).unwrap(), original_index);
        let mut next = Batch::open(objects).unwrap();
        let c = next.put(b"third chunk").unwrap();
        next.put(b"fourth chunk keeps this a pack").unwrap();
        next.finish().unwrap();
        assert_eq!(first.read(&a).unwrap(), b"first packed chunk");
        assert!(first.read(&c).is_err(), "snapshots do not silently reload");
        assert_eq!(
            Reader::open(objects).unwrap().read(&c).unwrap(),
            b"third chunk"
        );

        let mut malformed = original_index.clone();
        malformed[HEADER_BYTES] ^= 1;
        assert!(
            decode_index(malformed.clone()).is_err(),
            "checksum is mandatory"
        );
        let offset_start = HEADER_BYTES + 8 + PACK_RECORD_BYTES + 32 + 4;
        malformed = original_index.clone();
        malformed[offset_start..offset_start + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let checksum = blake3::hash(&malformed[HEADER_BYTES..]);
        malformed[20..HEADER_BYTES].copy_from_slice(checksum.as_bytes());
        assert!(
            decode_index(malformed.clone()).is_err(),
            "checked bounds reject overflow"
        );
        assert!(index_size(1, MAX_INDEX_BYTES).is_err());
        assert!(decode_index(original_index[..original_index.len() - 1].to_vec()).is_err());
        assert!(first.read("../pack-index").is_err());

        let location = first.locations().get(&a).unwrap().clone();
        let pack_path = objects.join(&location.pack);
        let mut payload = fs::read(&pack_path).unwrap();
        payload[location.offset as usize] ^= 1;
        overwrite(&pack_path, &payload);
        assert!(first.read(&a).is_err());
        let mut corrupted = Batch::open(objects).unwrap();
        assert!(corrupted.put(b"first packed chunk").is_err());
        assert!(corrupted.finish().is_err());
        overwrite(&pack_path, &payload[..payload.len() - 1]);
        assert!(first.read(&b).is_err(), "whole payload length is checked");

        let pending_fixture = Fixture::new();
        let mut pending = Batch::open(&pending_fixture.0).unwrap();
        pending.put(b"pending bytes").unwrap();
        let path = pending.pending.as_ref().unwrap().temporary.0.clone();
        overwrite(&path, b"damaged bytes");
        assert!(pending.put(b"pending bytes").is_err());
        assert!(pending.finish().is_err());
        assert!(!pending_fixture.0.join(INDEX_NAME).exists());
        assert!(!path.exists());

        let single_fixture = Fixture::new();
        let mut single = Batch::open(&single_fixture.0).unwrap();
        let address = single.put(b"one new chunk").unwrap();
        assert_eq!(single.put(b"one new chunk").unwrap(), address);
        assert!(!single_fixture.0.join(&address).exists());
        single.finish().unwrap();
        assert_eq!(
            fs::read(single_fixture.0.join(&address)).unwrap(),
            b"one new chunk"
        );
        assert!(!single_fixture.0.join(INDEX_NAME).exists());
        assert!(!single_fixture.0.join(FORMAT_NAME).exists());
        assert_eq!(
            PointReader::open(&single_fixture.0)
                .unwrap()
                .read(&address)
                .unwrap(),
            b"one new chunk"
        );
        let mut mixed = Batch::open(&single_fixture.0).unwrap();
        mixed.put(b"one new chunk").unwrap();
        mixed.put(b"first new packed chunk").unwrap();
        mixed.put(b"second new packed chunk").unwrap();
        mixed.finish().unwrap();
        let saved_index = fs::read(single_fixture.0.join(INDEX_NAME)).unwrap();
        let saved_marker = fs::read(single_fixture.0.join(FORMAT_NAME)).unwrap();
        let mut one_more = Batch::open(&single_fixture.0).unwrap();
        one_more.put(b"first new packed chunk").unwrap();
        let extra = one_more.put(b"only one additional chunk").unwrap();
        one_more.finish().unwrap();
        assert_eq!(
            fs::read(single_fixture.0.join(&extra)).unwrap(),
            b"only one additional chunk"
        );
        assert_eq!(
            fs::read(single_fixture.0.join(INDEX_NAME)).unwrap(),
            saved_index
        );
        assert_eq!(
            fs::read(single_fixture.0.join(FORMAT_NAME)).unwrap(),
            saved_marker
        );

        for phase in [FinishPhase::PayloadSynced, FinishPhase::PayloadPublished] {
            let loose_fixture = Fixture::new();
            let mut failed = Batch::open(&loose_fixture.0).unwrap();
            let address = failed.put(b"loose failure").unwrap();
            FINISH_FAULT.with(|fault| fault.set(Some(phase)));
            let error = failed.finish().unwrap_err().to_string();
            FINISH_FAULT.with(|fault| fault.set(None));
            assert_eq!(
                loose_fixture.0.join(&address).exists(),
                phase == FinishPhase::PayloadPublished
            );
            if phase == FinishPhase::PayloadPublished {
                assert!(error.contains("outcome unknown"));
                assert_eq!(
                    Reader::open(&loose_fixture.0)
                        .unwrap()
                        .read(&address)
                        .unwrap(),
                    b"loose failure"
                );
            }
            assert!(!loose_fixture.0.join(INDEX_NAME).exists());
            assert!(!loose_fixture.0.join(FORMAT_NAME).exists());
            let mut retry = Batch::open(&loose_fixture.0).unwrap();
            assert_eq!(retry.put(b"loose failure").unwrap(), address);
            retry.finish().unwrap();
            assert_eq!(
                Reader::open(&loose_fixture.0)
                    .unwrap()
                    .read(&address)
                    .unwrap(),
                b"loose failure"
            );
        }

        for phase in [
            FinishPhase::PayloadSynced,
            FinishPhase::PayloadPublished,
            FinishPhase::BeforeIndexRename,
            FinishPhase::IndexRenamed,
        ] {
            let failed_fixture = Fixture::new();
            let mut seed = Batch::open(&failed_fixture.0).unwrap();
            let baseline = seed.put(b"baseline").unwrap();
            seed.put(b"baseline companion").unwrap();
            seed.finish().unwrap();
            let before = Reader::open(&failed_fixture.0).unwrap();
            let mut failed = Batch::open(&failed_fixture.0).unwrap();
            let unpublished = failed.put(b"not indexed").unwrap();
            failed.put(b"not indexed companion").unwrap();
            FINISH_FAULT.with(|fault| fault.set(Some(phase)));
            let error = failed.finish().unwrap_err().to_string();
            FINISH_FAULT.with(|fault| fault.set(None));
            let cold = Reader::open(&failed_fixture.0).unwrap();
            assert_eq!(cold.read(&baseline).unwrap(), b"baseline");
            assert_eq!(before.read(&baseline).unwrap(), b"baseline");
            assert!(before.read(&unpublished).is_err());
            if phase == FinishPhase::IndexRenamed {
                assert!(error.contains("outcome unknown"));
                assert_eq!(cold.read(&unpublished).unwrap(), b"not indexed");
            } else {
                assert!(cold.read(&unpublished).is_err());
            }
            let payloads = fs::read_dir(&failed_fixture.0)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("pack-") && name != INDEX_NAME
                })
                .count();
            assert_eq!(
                payloads,
                if phase == FinishPhase::PayloadSynced {
                    1
                } else {
                    2
                },
                "published payloads must survive failure"
            );
            let mut retry = Batch::open(&failed_fixture.0).unwrap();
            assert_eq!(retry.put(b"not indexed").unwrap(), unpublished);
            retry.finish().unwrap();
            assert_eq!(
                Reader::open(&failed_fixture.0)
                    .unwrap()
                    .read(&unpublished)
                    .unwrap(),
                b"not indexed"
            );
        }

        // A content-publication failure must not advance the real Store head,
        // even when its index publication already took effect.
        let managed = Fixture::new();
        let source = managed.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("document"), b"original").unwrap();
        fs::write(source.join("companion"), b"original companion").unwrap();
        let store_path = managed.0.join("store");
        let store = crate::Store::init(&store_path).unwrap();
        let workspace = store
            .import(&source, "main", crate::CopyMode::Copy)
            .unwrap();
        let baseline_state = fs::read(store_path.join("state.json")).unwrap();
        for phase in [
            FinishPhase::PayloadSynced,
            FinishPhase::PayloadPublished,
            FinishPhase::BeforeIndexRename,
            FinishPhase::IndexRenamed,
        ] {
            fs::write(
                workspace.path.join("document"),
                format!("candidate {phase:?}"),
            )
            .unwrap();
            fs::write(
                workspace.path.join("companion"),
                format!("companion candidate {phase:?}"),
            )
            .unwrap();
            FINISH_FAULT.with(|fault| fault.set(Some(phase)));
            let outcome = store.checkpoint("main", "must not publish", None);
            FINISH_FAULT.with(|fault| fault.set(None));
            assert!(outcome.is_err());
            assert_eq!(
                fs::read(store_path.join("state.json")).unwrap(),
                baseline_state
            );
            let reopened = crate::Store::open(&store_path).unwrap();
            assert_eq!(reopened.read("main", "document").unwrap(), b"original");
            assert!(reopened.doctor().unwrap().healthy);
        }
        assert!(store.checkpoint("main", "recover", None).unwrap().committed);
        assert!(store.doctor().unwrap().healthy);

        let missing_fixture = Fixture::new();
        let mut seed = Batch::open(&missing_fixture.0).unwrap();
        seed.put(b"index must persist").unwrap();
        seed.put(b"index companion").unwrap();
        seed.finish().unwrap();
        fs::remove_file(missing_fixture.0.join(INDEX_NAME)).unwrap();
        assert!(Reader::open(&missing_fixture.0).is_err());
        assert!(PointReader::open(&missing_fixture.0).is_err());
        assert!(
            Snapshot::load_with(|name| platform::open_regular(&missing_fixture.0.join(name)))
                .is_err()
        );
        assert!(Batch::open(&missing_fixture.0).is_err());

        let initialization_fixture = Fixture::new();
        fs::write(
            initialization_fixture.0.join(INDEX_NAME),
            encode_index(&Catalog::default()).unwrap(),
        )
        .unwrap();
        let mut restart = Batch::open(&initialization_fixture.0).unwrap();
        let recovered = restart.put(b"restart initialization").unwrap();
        restart.put(b"restart companion").unwrap();
        restart.finish().unwrap();
        assert_eq!(
            Reader::open(&initialization_fixture.0)
                .unwrap()
                .read(&recovered)
                .unwrap(),
            b"restart initialization"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let symlink_fixture = Fixture::new();
            let mut seed = Batch::open(&symlink_fixture.0).unwrap();
            let address = seed.put(b"no symlink payload").unwrap();
            seed.put(b"symlink fixture companion").unwrap();
            seed.finish().unwrap();
            let reader = Reader::open(&symlink_fixture.0).unwrap();
            let pack = reader.locations()[&address].pack.clone();
            let pack_path = symlink_fixture.0.join(pack);
            let moved = symlink_fixture.0.join("moved-payload");
            fs::rename(&pack_path, &moved).unwrap();
            symlink(&moved, &pack_path).unwrap();
            assert!(reader.read(&address).is_err());
            assert!(reader.length(&address).is_err());
            assert!(
                PointReader::open(&symlink_fixture.0)
                    .unwrap()
                    .read(&address)
                    .is_err()
            );
            fs::remove_file(symlink_fixture.0.join(LOCK_NAME)).unwrap();
            symlink(&moved, symlink_fixture.0.join(LOCK_NAME)).unwrap();
            assert!(Batch::open(&symlink_fixture.0).is_err());
        }
    }
}
