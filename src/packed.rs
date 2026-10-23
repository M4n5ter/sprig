//! Immutable payload packs with an atomically replaced, bounded index.
//!
//! A snapshot owns exact pack locations, not a mutable process-wide cache.
//! Packs are never modified or removed here. An unsuccessful finish can leave
//! an unindexed pack or an already-published index; it never rolls either back.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::content::Result;
use crate::platform;

const INDEX_NAME: &str = "pack-index";
const LOCK_NAME: &str = ".pack-lock";
const FORMAT_NAME: &str = ".pack-format";
const FORMAT_BYTES: &[u8] = b"sprig immutable packs 1\n";
const MAGIC: &[u8; 8] = b"SPRGPKIX";
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 8 + 4 + 8 + 32;
const PACK_RECORD_BYTES: usize = 32 + 8;
const OBJECT_RECORD_BYTES: usize = 32 + 4 + 8 + 4;
const MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;
const MAX_CHUNK_BYTES: u64 = 256 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Location {
    pub(crate) pack: String,
    pub(crate) offset: u64,
    pub(crate) length: u64,
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

    #[cfg(any(test, all(target_os = "linux", feature = "projection")))]
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
    objects: PathBuf,
    snapshot: Snapshot,
}

impl Reader {
    pub(crate) fn open(objects: &Path) -> Result<Self> {
        validate_directory_chain(objects)?;
        Ok(Self {
            objects: objects.to_owned(),
            snapshot: Snapshot::load_with(|name| platform::open_regular(&objects.join(name)))?,
        })
    }

    pub(crate) fn read(&self, hash: &str) -> Result<Vec<u8>> {
        self.snapshot.read_with(hash, |name| {
            platform::open_regular(&self.objects.join(name))
        })
    }

    #[cfg(test)]
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
    objects: PathBuf,
    index: Option<PointIndex>,
}

impl PointReader {
    pub(crate) fn open(objects: &Path) -> Result<Self> {
        validate_directory_chain(objects)?;
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
                    let pack_length =
                        u64::from_le_bytes(pack[32..40].try_into().expect("fixed field"));
                    validate_extent(offset, length, pack_length)?;
                    return Ok(Some(Location {
                        pack: format!("pack-{}", hex(pack[..32].try_into().expect("fixed field"))),
                        offset,
                        length,
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
    additions: BTreeMap<String, (u64, u64)>,
    failed: bool,
    // Keep the lock inode and handle for the entire batch. Never unlink it.
    _lock: File,
}

impl Batch {
    /// The caller creates the object directory before opening a batch.
    pub(crate) fn open(objects: &Path) -> Result<Self> {
        validate_directory_chain(objects)?;
        let path = objects.join(LOCK_NAME);
        let lock = match platform::create_new(&path, 0o600) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                platform::open_read_write(&path)?
            }
            Err(error) => return Err(error.into()),
        };
        lock.try_lock()
            .map_err(|error| io::Error::other(format!("object_store_busy: {error}")))?;
        // Read only after taking the writer lock, to avoid lost index updates.
        Ok(Self {
            reader: Reader::open(objects)?,
            pending: None,
            additions: BTreeMap::new(),
            failed: false,
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
        if let Some(&(offset, length)) = self.additions.get(&hash) {
            let pending = self.pending.as_ref().expect("pending chunk has a payload");
            let location = Location {
                pack: pending.name.clone(),
                offset,
                length,
                pack_length: pending.length,
            };
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
        // One new chunk uses the loose layout without growing the index. As
        // soon as a second chunk requires a pack, enforce its eventual index
        // bound before appending or growing an unbounded batch catalog.
        if !self.additions.is_empty() {
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
            let id = unique_id();
            let temporary = self.reader.objects.join(format!(".pack-tmp-{id}"));
            let file = platform::create_new(&temporary, 0o600)?;
            self.pending = Some(PendingPack {
                file,
                temporary: Temporary(temporary),
                name: format!("pack-{id}"),
                length: 0,
            });
        }
        let pending = self.pending.as_mut().expect("payload created");
        let offset = pending.length;
        let length = bytes.len() as u64;
        let next_length = offset
            .checked_add(length)
            .ok_or_else(|| invalid_error("pack payload length overflow"))?;
        pending.file.write_all(bytes)?;
        pending.length = next_length;
        self.additions.insert(hash.clone(), (offset, length));
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
            let (hash, &(offset, length)) = self.additions.first_key_value().expect("one chunk");
            debug_assert_eq!((offset, length), (0, pending.length));
            return publish_loose(&self.reader.objects, pending, hash);
        }
        // Dedup-only batches never expand the catalog. A writing batch does
        // so once, only when publication requires rewriting the index.
        let mut catalog = Catalog {
            locations: self.reader.snapshot.locations(),
            packs: self.reader.snapshot.pack_sizes(),
        };
        catalog.packs.insert(pending.name.clone(), pending.length);
        for (hash, (offset, length)) in std::mem::take(&mut self.additions) {
            catalog.locations.insert(
                hash,
                Location {
                    pack: pending.name.clone(),
                    offset,
                    length,
                    pack_length: pending.length,
                },
            );
        }
        // Encoding, including the size cap, happens before any publication.
        let encoded = encode_index(&catalog)?;
        initialize_format(&self.reader.objects)?;
        platform::protect_object(&pending.file)?;
        pending.file.sync_all()?;
        #[cfg(test)]
        fail_finish(FinishPhase::PayloadSynced)?;
        drop(pending.file);
        // Hard-link publication is NOREPLACE on all supported platforms.
        // A collision is an error, never permission to replace an old pack.
        fs::hard_link(
            &pending.temporary.0,
            self.reader.objects.join(&pending.name),
        )?;
        fs::remove_file(&pending.temporary.0)?;
        platform::sync_directory(&self.reader.objects)?;
        #[cfg(test)]
        fail_finish(FinishPhase::PayloadPublished)?;

        let temporary = self
            .reader
            .objects
            .join(format!(".pack-index-tmp-{}", unique_id()));
        let mut file = platform::create_new(&temporary, 0o600)?;
        let _cleanup = Temporary(temporary.clone());
        file.write_all(&encoded)?;
        platform::protect_object(&file)?;
        file.sync_all()?;
        drop(file);
        #[cfg(test)]
        fail_finish(FinishPhase::BeforeIndexRename)?;
        platform::replace_immutable_file(&temporary, &self.reader.objects.join(INDEX_NAME))
            .map_err(|error| {
                io::Error::other(format!(
                    "pack index publication failed; outcome may be unknown: {error}"
                ))
            })?;
        let final_barrier = || -> Result<()> {
            #[cfg(test)]
            fail_finish(FinishPhase::IndexRenamed)?;
            platform::sync_directory(&self.reader.objects)?;
            Ok(())
        };
        final_barrier().map_err(|error| {
            io::Error::other(format!("pack index publication outcome unknown: {error}"))
        })?;
        Ok(())
    }
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
    let mut bytes = vec![0; location.length as usize];
    file.read_exact(&mut bytes)?;
    if blake3::hash(&bytes).to_hex().as_str() != hash {
        return invalid(format!("corrupt packed object: {hash}"));
    }
    Ok(bytes)
}

fn validate_location(location: &Location) -> Result<()> {
    validate_extent(location.offset, location.length, location.pack_length)?;
    validate_pack_name(&location.pack)
}

fn validate_extent(offset: u64, length: u64, pack_length: u64) -> Result<()> {
    if length == 0
        || length > MAX_CHUNK_BYTES
        || pack_length == 0
        || offset
            .checked_add(length)
            .is_none_or(|end| end > pack_length)
    {
        return invalid("packed object has invalid length or out-of-bounds offset");
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
        validate_extent(offset, length, pack_length)?;
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
    fn packed_batches_preserve_snapshots_verify_dedup_and_reject_damage() {
        // The in-memory read representation changes, not the serialized
        // format. Exercise every binary-search position, gaps and expansion.
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
