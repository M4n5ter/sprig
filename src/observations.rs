//! Immutable physical-file observations, addressed by their canonical JSON.
//!
//! The codec has no document-size or entry-count format limit. A caller owns
//! one [`Meter`] per operation, whose configurable budget limits actual decoded
//! work across distinct references. Neither declared lengths nor entry counts
//! are used to allocate storage. Verification retains only one entry and the
//! preceding path; reading additionally builds the requested ordinary map.
//!
//! Store paths must be trusted and quiescent, just like content-object paths.
//! Successful verification is reused only within the caller's operation. The
//! caller must discard its meter before permitting another store mutation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::content::{Observation, Observed};
use crate::{Result, platform};

const FORMAT: &str = "sprig_observation_json_zstd";
const VERSION: u32 = 1;
const DOMAIN: &[u8] = b"sprig-observation-json-v1\0";
// This bounds codec working memory, not the total decoded document size.
const WINDOW_LOG: u32 = 20;
static NONCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublicationPhase {
    TemporaryWritten,
    TemporarySynced,
    Published,
    BeforeDirectorySync,
    DedupBeforeSync,
}

#[cfg(test)]
thread_local! {
    static PUBLICATION_FAULT: std::cell::Cell<Option<PublicationPhase>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn set_publication_fault(phase: Option<PublicationPhase>) {
    PUBLICATION_FAULT.set(phase);
}

#[cfg(test)]
fn publication_point(phase: PublicationPhase) -> Result<()> {
    if PUBLICATION_FAULT.get() == Some(phase) {
        return Err(
            io::Error::other(format!("injected observation publication fault: {phase:?}")).into(),
        );
    }
    Ok(())
}

/// The content address and verified sizes of one canonical observation map.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRef {
    pub format: String,
    pub version: u32,
    pub hash: String,
    pub raw_bytes: u64,
    pub entries: u64,
}

/// Resource policy for one operation, not a permanent on-disk format limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationBudget {
    pub max_raw_bytes: u64,
    pub max_entries: u64,
    /// Actual compressed reads and writes, including repeated reads.
    pub max_encoded_bytes: u64,
}

impl Default for ObservationBudget {
    fn default() -> Self {
        Self {
            max_raw_bytes: 256 * 1024 * 1024,
            max_entries: 1_000_000,
            max_encoded_bytes: 512 * 1024 * 1024,
        }
    }
}

/// Actual decoded work and successfully verified references for one operation.
///
/// Decoded bytes/entries are charged once per content hash, including a map
/// measured before publication. Failed first reads charge actual decoded work;
/// later reads of an already-accounted hash do not charge those two counters
/// again, even if they fail. Encoded IO is always charged. Store operations use
/// one trusted directory and return immediately on errors. No maps are cached.
#[derive(Debug)]
pub struct Meter {
    budget: ObservationBudget,
    raw_bytes: u64,
    entries: u64,
    encoded_bytes: u64,
    charged: BTreeMap<String, ObservationRef>,
    verified: BTreeSet<(PathBuf, String)>,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new(ObservationBudget::default())
    }
}

impl Meter {
    pub fn new(budget: ObservationBudget) -> Self {
        Self {
            budget,
            raw_bytes: 0,
            entries: 0,
            encoded_bytes: 0,
            charged: BTreeMap::new(),
            verified: BTreeSet::new(),
        }
    }

    #[cfg(test)]
    pub fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }

    #[cfg(test)]
    pub fn entries(&self) -> u64 {
        self.entries
    }

    #[cfg(test)]
    pub fn encoded_bytes(&self) -> u64 {
        self.encoded_bytes
    }

    /// Charges actual compressed IO, also usable by maintenance inventory reads.
    /// Unlike decoded distinct-reference accounting, repeated IO is not free.
    pub fn charge_encoded(&mut self, bytes: u64) -> Result<()> {
        self.encoded_bytes = self
            .encoded_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("observation encoded-byte counter overflow"))?;
        if self.encoded_bytes > self.budget.max_encoded_bytes {
            return Err(invalid("observation encoded-byte budget exceeded"));
        }
        Ok(())
    }

    fn encoded_remaining(&self) -> u64 {
        self.budget
            .max_encoded_bytes
            .saturating_sub(self.encoded_bytes)
    }

    fn known(&self, reference: &ObservationRef) -> Result<bool> {
        if let Some(previous) = self.charged.get(&reference.hash) {
            if previous != reference {
                return Err(invalid(
                    "inconsistent observation reference for the same hash",
                ));
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn limits(&self, known: bool) -> ObservationBudget {
        if known {
            self.budget
        } else {
            ObservationBudget {
                max_raw_bytes: self.budget.max_raw_bytes.saturating_sub(self.raw_bytes),
                max_entries: self.budget.max_entries.saturating_sub(self.entries),
                max_encoded_bytes: self.encoded_remaining(),
            }
        }
    }

    fn charge(&mut self, raw_bytes: u64, entries: u64) -> Result<()> {
        let raw_bytes = self
            .raw_bytes
            .checked_add(raw_bytes)
            .ok_or_else(|| invalid("observation decoded-byte counter overflow"))?;
        let entries = self
            .entries
            .checked_add(entries)
            .ok_or_else(|| invalid("observation entry counter overflow"))?;
        // Retain actual consumption even when the one-byte EOF probe detects
        // that a decoder exceeded its remaining allowance.
        self.raw_bytes = raw_bytes;
        self.entries = entries;
        if raw_bytes > self.budget.max_raw_bytes {
            return Err(invalid("observation decoded-byte budget exceeded"));
        }
        if entries > self.budget.max_entries {
            return Err(invalid("observation entry budget exceeded"));
        }
        Ok(())
    }

    fn remember(&mut self, directory: &Path, reference: &ObservationRef) {
        self.charged
            .insert(reference.hash.clone(), reference.clone());
        self.verified
            .insert((directory.to_owned(), reference.hash.clone()));
    }
}

fn invalid(message: impl Into<String>) -> crate::Error {
    invalid_io(message).into()
}

fn invalid_io(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Checks descriptor syntax only; does not trust its sizes or read its blob.
pub fn validate_descriptor(reference: &ObservationRef) -> Result<()> {
    if reference.format != FORMAT || reference.version != VERSION {
        return Err(invalid("unsupported observation format or version"));
    }
    validate_hash(&reference.hash)?;
    if reference.raw_bytes < 2 || reference.entries > reference.raw_bytes {
        return Err(invalid("invalid observation descriptor counts"));
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid("invalid observation content address"));
    }
    Ok(())
}

/// A deterministic descriptor, not permission to omit the corresponding blob.
/// Store initialization durably writes this map before persisting references.
pub(crate) fn empty_reference() -> ObservationRef {
    let mut hash = new_hasher();
    hash.update(b"{}");
    ObservationRef {
        format: FORMAT.to_owned(),
        version: VERSION,
        hash: hash.finalize().to_hex().to_string(),
        raw_bytes: 2,
        entries: 0,
    }
}

fn new_hasher() -> blake3::Hasher {
    let mut hash = blake3::Hasher::new();
    hash.update(DOMAIN);
    hash
}

fn directory_path(directory: &Path, create: bool) -> Result<PathBuf> {
    match platform::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(invalid("observations path must be a real directory")),
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
            match platform::create_dir(directory, 0o700) {
                Ok(()) => {
                    if let Some(parent) = directory.parent() {
                        platform::sync_directory(if parent.as_os_str().is_empty() {
                            Path::new(".")
                        } else {
                            parent
                        })?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            let metadata = platform::symlink_metadata(directory)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(invalid("observations path must be a real directory"));
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(fs::canonicalize(directory)?)
}

fn blob_path(directory: &Path, reference: &ObservationRef) -> PathBuf {
    directory.join(format!("{}.zst", reference.hash))
}

fn validate_path(path: &str) -> Result<()> {
    if !path.is_empty() {
        platform::validate_relative_path(path)?;
        if path.contains('\0')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(invalid("invalid relative observation path"));
        }
    }
    Ok(())
}

struct DigestWriter<W> {
    writer: W,
    hash: blake3::Hasher,
    bytes: u64,
    limit: u64,
}

impl<W: Write> DigestWriter<W> {
    fn new(writer: W, limit: u64) -> Self {
        Self {
            writer,
            hash: new_hasher(),
            bytes: 0,
            limit,
        }
    }
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(bytes.len())
            .map_err(|_| invalid_io("observation byte count overflow"))?;
        let next = self
            .bytes
            .checked_add(length)
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| invalid_io("observation decoded-byte budget exceeded"))?;
        // All-or-error writes make the counter and hash correspond exactly to
        // bytes accepted by the canonical output stream.
        self.writer.write_all(bytes)?;
        self.hash.update(bytes);
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn emit(observed: &Observed, output: &mut impl Write, entry_limit: u64) -> Result<u64> {
    output.write_all(b"{")?;
    let mut entries = 0_u64;
    for (path, observation) in observed {
        if entries == entry_limit {
            return Err(invalid("observation entry budget exceeded"));
        }
        validate_path(path)?;
        if entries != 0 {
            output.write_all(b",")?;
        }
        serde_json::to_writer(&mut *output, path)?;
        output.write_all(b":")?;
        serde_json::to_writer(&mut *output, observation)?;
        entries = entries
            .checked_add(1)
            .ok_or_else(|| invalid("observation entry counter overflow"))?;
    }
    output.write_all(b"}")?;
    Ok(entries)
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        // Only the private staging name is removed. A published content name
        // is never deleted on a failure, including a failed directory sync.
        let _ = fs::remove_file(&self.0);
    }
}

/// Durably publishes a canonical map in the dedicated observations directory.
/// Existing addresses must decode to the same canonical content; alternative
/// valid compressed representations are accepted, and corruption is not fixed
/// by replacing or overwriting an existing blob.
pub fn put(directory: &Path, observed: &Observed, meter: &mut Meter) -> Result<ObservationRef> {
    // The first pass discovers the content address without buffering JSON.
    // Per-map policy permits already-accounted references even when the
    // aggregate operation has exhausted its allowance.
    let mut digest = DigestWriter::new(io::sink(), meter.budget.max_raw_bytes);
    let entries = emit(observed, &mut digest, meter.budget.max_entries)?;
    let reference = ObservationRef {
        format: FORMAT.to_owned(),
        version: VERSION,
        hash: digest.hash.finalize().to_hex().to_string(),
        raw_bytes: digest.bytes,
        entries,
    };
    let known = meter.known(&reference)?;
    if !known {
        meter.charge(reference.raw_bytes, reference.entries)?;
        // These counts came from actual canonical serialization, not a
        // descriptor supplied by the store. Account only once if publishing
        // fails and a caller elects to retry in the same operation.
        meter
            .charged
            .insert(reference.hash.clone(), reference.clone());
    }
    let directory = directory_path(directory, true)?;
    if meter
        .verified
        .contains(&(directory.clone(), reference.hash.clone()))
    {
        #[cfg(test)]
        publication_point(PublicationPhase::DedupBeforeSync)?;
        platform::sync_file(&blob_path(&directory, &reference))?;
        platform::sync_directory(&directory)?;
        return Ok(reference);
    }
    let target = blob_path(&directory, &reference);
    match platform::symlink_metadata(&target) {
        Ok(_) => {
            decode(&directory, &reference, meter, false)?;
            #[cfg(test)]
            publication_point(PublicationPhase::DedupBeforeSync)?;
            platform::sync_file(&target)?;
            platform::sync_directory(&directory)?;
            return Ok(reference);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let (temporary, mut file) = loop {
        let path = directory.join(format!(
            ".observation-{}-{}.tmp",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        match platform::create_new(&path, 0o600) {
            Ok(file) => break (Temporary(path), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let mut encoded = EncodedWriter {
        writer: &mut file,
        bytes: 0,
        limit: meter.encoded_remaining(),
    };
    let encoded_result = (|| -> Result<()> {
        let mut encoder = zstd::stream::write::Encoder::new(&mut encoded, 1)?;
        encoder.window_log(WINDOW_LOG)?;
        encoder.include_checksum(true)?;
        // Do not pledge an arbitrarily large source size: a fixed decoder
        // window remains sufficient for every permitted document size.
        let mut output = DigestWriter::new(&mut encoder, reference.raw_bytes);
        let written_entries = emit(observed, &mut output, reference.entries)?;
        if output.bytes != reference.raw_bytes
            || written_entries != reference.entries
            || output.hash.finalize().to_hex().as_str() != reference.hash
        {
            return Err(invalid("observation changed during canonical encoding"));
        }
        drop(output);
        encoder.finish()?;
        Ok(())
    })();
    meter.charge_encoded(encoded.bytes)?;
    encoded_result?;
    #[cfg(test)]
    publication_point(PublicationPhase::TemporaryWritten)?;
    platform::protect_object(&file)?;
    file.sync_all()?;
    #[cfg(test)]
    publication_point(PublicationPhase::TemporarySynced)?;
    drop(file);
    match platform::hard_link(&temporary.0, &target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            decode(&directory, &reference, meter, false)?;
            #[cfg(test)]
            publication_point(PublicationPhase::DedupBeforeSync)?;
            platform::sync_file(&target)?;
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(test)]
    publication_point(PublicationPhase::Published)?;
    fs::remove_file(&temporary.0)?;
    #[cfg(test)]
    publication_point(PublicationPhase::BeforeDirectorySync)?;
    platform::sync_directory(&directory)?;
    meter.remember(&directory, &reference);
    Ok(reference)
}

/// Returns the owned map, re-reading rather than caching previously read maps.
pub fn read(directory: &Path, reference: &ObservationRef, meter: &mut Meter) -> Result<Observed> {
    validate_descriptor(reference)?;
    let directory = directory_path(directory, false)?;
    Ok(decode(&directory, reference, meter, true)?.expect("retaining decoder returns a map"))
}

/// Verifies an entire frame and canonical map without retaining the map.
pub fn verify(directory: &Path, reference: &ObservationRef, meter: &mut Meter) -> Result<()> {
    validate_descriptor(reference)?;
    let directory = directory_path(directory, false)?;
    if meter.known(reference)?
        && meter
            .verified
            .contains(&(directory.clone(), reference.hash.clone()))
    {
        return Ok(());
    }
    decode(&directory, reference, meter, false)?;
    Ok(())
}

struct Decoded<R> {
    reader: R,
    hash: blake3::Hasher,
    bytes: u64,
    limit: u64,
}

impl<R: Read> Read for Decoded<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // Probe at most one byte beyond the allowance to distinguish exact
        // EOF from expansion. No allocation follows an untrusted length.
        let remaining = self.limit.saturating_sub(self.bytes);
        let length = buffer
            .len()
            .min(usize::try_from(remaining.saturating_add(1)).unwrap_or(usize::MAX));
        let read = self.reader.read(&mut buffer[..length])?;
        self.bytes = self
            .bytes
            .checked_add(read as u64)
            .ok_or_else(|| invalid_io("observation decoded-byte counter overflow"))?;
        if self.bytes > self.limit {
            return Err(invalid_io(
                "observation decoded-byte budget or declared size exceeded",
            ));
        }
        self.hash.update(&buffer[..read]);
        Ok(read)
    }
}

struct OrderedVisitor<'a> {
    canonical: &'a mut DigestWriter<io::Sink>,
    entries: &'a mut u64,
    entry_limit: u64,
    retain: bool,
}

impl<'de> Visitor<'de> for OrderedVisitor<'_> {
    type Value = Option<Observed>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical strictly sorted observation map")
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut previous: Option<String> = None;
        let mut retained = self.retain.then(Observed::new);
        self.canonical.write_all(b"{").map_err(de::Error::custom)?;
        while let Some(path) = map.next_key::<String>()? {
            if *self.entries == self.entry_limit {
                return Err(de::Error::custom(
                    "observation entry budget or declared count exceeded",
                ));
            }
            if previous.as_ref().is_some_and(|previous| previous >= &path) {
                return Err(de::Error::custom(
                    "observation paths must be strictly sorted and unique",
                ));
            }
            validate_path(&path).map_err(de::Error::custom)?;
            // Observation rejects unknown/duplicate fields and decodes the
            // complete canonical 32-hex-digit u128 physical identity.
            let observation = map.next_value::<Observation>()?;
            if *self.entries != 0 {
                self.canonical.write_all(b",").map_err(de::Error::custom)?;
            }
            serde_json::to_writer(&mut *self.canonical, &path).map_err(de::Error::custom)?;
            self.canonical.write_all(b":").map_err(de::Error::custom)?;
            serde_json::to_writer(&mut *self.canonical, &observation).map_err(de::Error::custom)?;
            *self.entries = self
                .entries
                .checked_add(1)
                .ok_or_else(|| de::Error::custom("observation entry counter overflow"))?;
            if let Some(retained) = &mut retained {
                retained.insert(path.clone(), observation);
            }
            previous = Some(path);
        }
        self.canonical.write_all(b"}").map_err(de::Error::custom)?;
        Ok(retained)
    }
}

/// Inspects an orphan by its filename hash, deriving counts from the decoded
/// canonical stream rather than inventing or trusting a size descriptor.
pub fn inspect(directory: &Path, hash: &str, meter: &mut Meter) -> Result<ObservationRef> {
    validate_hash(hash)?;
    let directory = directory_path(directory, false)?;
    if meter
        .verified
        .contains(&(directory.clone(), hash.to_owned()))
    {
        return Ok(meter
            .charged
            .get(hash)
            .expect("verified reference has counts")
            .clone());
    }
    Ok(decode_hash(&directory, hash, None, meter, false)?.1)
}

struct EncodedWriter<W> {
    writer: W,
    bytes: u64,
    limit: u64,
}

impl<W: Write> Write for EncodedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.bytes);
        if remaining == 0 {
            return Err(invalid_io("observation encoded-byte budget exceeded"));
        }
        let length = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let written = self.writer.write(&buffer[..length])?;
        self.bytes = self
            .bytes
            .checked_add(written as u64)
            .ok_or_else(|| invalid_io("observation encoded-byte counter overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct EncodedReader<R> {
    reader: R,
    bytes: u64,
    limit: u64,
}

impl<R: Read> Read for EncodedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // The only permitted overshoot is a one-byte EOF probe. Compressed
        // streams producing no output still exhaust this operation policy.
        let remaining = self.limit.saturating_sub(self.bytes);
        let length = buffer
            .len()
            .min(usize::try_from(remaining.saturating_add(1)).unwrap_or(usize::MAX));
        let read = self.reader.read(&mut buffer[..length])?;
        self.bytes = self
            .bytes
            .checked_add(read as u64)
            .ok_or_else(|| invalid_io("observation encoded-byte counter overflow"))?;
        if self.bytes > self.limit {
            return Err(invalid_io("observation encoded-byte budget exceeded"));
        }
        Ok(read)
    }
}

fn decode(
    directory: &Path,
    reference: &ObservationRef,
    meter: &mut Meter,
    retain: bool,
) -> Result<Option<Observed>> {
    validate_descriptor(reference)?;
    Ok(decode_hash(directory, &reference.hash, Some(reference), meter, retain)?.0)
}

fn decode_hash(
    directory: &Path,
    hash: &str,
    expected: Option<&ObservationRef>,
    meter: &mut Meter,
    retain: bool,
) -> Result<(Option<Observed>, ObservationRef)> {
    let known = match expected {
        Some(reference) => meter.known(reference)?,
        None => meter.charged.contains_key(hash),
    };
    let limits = meter.limits(known);
    let file = platform::open_regular(&directory.join(format!("{hash}.zst")))?;
    let encoded_bytes = platform::file_metadata(&file)?.len();
    let mut encoded = EncodedReader {
        reader: file.take(encoded_bytes),
        bytes: 0,
        limit: meter.encoded_remaining(),
    };
    let mut raw_bytes = 0_u64;
    let mut entries = 0_u64;
    let result = (|| -> Result<(Option<Observed>, ObservationRef)> {
        // Read from the actual file extent, never a descriptor-derived
        // allocation. Fixed-size buffers service arbitrarily large files.
        let mut compressed = BufReader::new(&mut encoded);
        let header = compressed.fill_buf()?;
        if header.len() < 5 || header[..4] != [0x28, 0xb5, 0x2f, 0xfd] || header[4] & 0x04 == 0 {
            return Err(invalid(
                "observations require an ordinary checksummed zstd frame",
            ));
        }
        let mut decoder = zstd::stream::read::Decoder::with_buffer(compressed)?.single_frame();
        decoder.window_log_max(WINDOW_LOG)?;
        let mut decoded = Decoded {
            reader: decoder,
            hash: new_hasher(),
            bytes: 0,
            limit: limits
                .max_raw_bytes
                .min(expected.map_or(u64::MAX, |reference| reference.raw_bytes)),
        };
        let mut canonical = DigestWriter::new(io::sink(), limits.max_raw_bytes);
        let parsed = (|| -> Result<Option<Observed>> {
            // serde's bytewise IoRead does not cause one codec call or hash
            // update per JSON byte because the decoded reader is buffered.
            let mut input = BufReader::new(&mut decoded);
            let mut parser = serde_json::Deserializer::from_reader(&mut input);
            let observed = parser.deserialize_map(OrderedVisitor {
                canonical: &mut canonical,
                entries: &mut entries,
                entry_limit: limits
                    .max_entries
                    .min(expected.map_or(u64::MAX, |reference| reference.entries)),
                retain,
            })?;
            // Read decoded EOF to validate the frame epilogue and checksum.
            parser.end()?;
            Ok(observed)
        })();
        raw_bytes = decoded.bytes;
        let observed = parsed?;
        let raw_hash = decoded.hash.finalize();
        if canonical.bytes != decoded.bytes || canonical.hash.finalize() != raw_hash {
            return Err(invalid("observation JSON is not canonical"));
        }
        if raw_hash.to_hex().as_str() != hash {
            return Err(invalid("observation content hash mismatch"));
        }
        let reference = ObservationRef {
            format: FORMAT.to_owned(),
            version: VERSION,
            hash: hash.to_owned(),
            raw_bytes,
            entries,
        };
        if expected.is_some_and(|expected| expected != &reference) {
            return Err(invalid(
                "observation decoded sizes do not match the descriptor",
            ));
        }
        let mut compressed = decoded.reader.finish();
        if !compressed.fill_buf()?.is_empty() {
            return Err(invalid("trailing bytes or frames after observation frame"));
        }
        drop(compressed);
        if encoded.reader.limit() != 0 {
            return Err(invalid("observation file ended before its opened length"));
        }
        // The Take wrapper bounds input to the opened file extent. Metadata
        // plus one bounded probe rejects growth rather than silently ignoring it.
        if platform::file_metadata(encoded.reader.get_ref())?.len() != encoded_bytes {
            return Err(invalid("observation file changed while being read"));
        }
        let read = encoded.reader.get_mut().read(&mut [0_u8; 1])?;
        encoded.bytes = encoded
            .bytes
            .checked_add(read as u64)
            .ok_or_else(|| invalid("observation encoded-byte counter overflow"))?;
        if read != 0 {
            return Err(invalid("observation file changed while being read"));
        }
        Ok((observed, reference))
    })();
    // Every compressed read is real work, even when a successful reference
    // was decoded previously in order to return another owned map.
    let encoded_charge = meter.charge_encoded(encoded.bytes);
    let raw_charge = if known {
        Ok(())
    } else {
        meter.charge(raw_bytes, entries)
    };
    encoded_charge?;
    raw_charge?;
    let (observed, reference) = result?;
    meter.known(&reference)?;
    meter.remember(directory, &reference);
    Ok((observed, reference))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirectory(PathBuf);

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            // Unix immutable files remain removable through their directory.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temporary() -> TempDirectory {
        let path = std::env::temp_dir().join(format!(
            "sprig-observations-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        platform::create_dir(&path, 0o700).unwrap();
        TempDirectory(path)
    }

    fn fixture() -> Observed {
        [
            ("", "root", None, 0, 0),
            ("a", "id-\"-\\-\n", Some("group-雪"), u64::MAX, u128::MAX),
            ("é-雪", "unicode", Some("group-雪"), 42, 1_u128 << 100),
        ]
        .into_iter()
        .map(|(path, id, link_group, device, inode)| {
            (
                path.to_owned(),
                Observation {
                    id: id.to_owned(),
                    link_group: link_group.map(str::to_owned),
                    device,
                    inode,
                },
            )
        })
        .collect()
    }

    fn descriptor(raw: &[u8], entries: u64) -> ObservationRef {
        let mut hash = new_hasher();
        hash.update(raw);
        ObservationRef {
            format: FORMAT.to_owned(),
            version: VERSION,
            hash: hash.finalize().to_hex().to_string(),
            raw_bytes: raw.len() as u64,
            entries,
        }
    }

    fn encoded(raw: &[u8], level: i32) -> Vec<u8> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
        encoder.window_log(WINDOW_LOG).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    fn store_raw(directory: &Path, raw: &[u8], entries: u64) -> ObservationRef {
        let reference = descriptor(raw, entries);
        fs::write(blob_path(directory, &reference), encoded(raw, 1)).unwrap();
        reference
    }

    #[test]
    fn roundtrip_empty_wide_identity_and_semantic_dedup() {
        let temp = temporary();
        let directory = temp.0.join("observations");
        let observed = fixture();
        let mut meter = Meter::default();
        let reference = put(&directory, &observed, &mut meter).unwrap();
        assert_eq!(
            reference,
            descriptor(&serde_json::to_vec(&observed).unwrap(), 3)
        );
        assert_eq!(read(&directory, &reference, &mut meter).unwrap(), observed);
        verify(&directory, &reference, &mut meter).unwrap();
        assert_eq!(put(&directory, &observed, &mut meter).unwrap(), reference);
        assert_eq!(meter.raw_bytes(), reference.raw_bytes);
        assert_eq!(meter.entries(), 3);
        let empty = put(&directory, &Observed::new(), &mut meter).unwrap();
        assert_eq!(empty, empty_reference());
        assert!(read(&directory, &empty, &mut meter).unwrap().is_empty());
        assert_eq!(meter.raw_bytes(), reference.raw_bytes + 2);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);

        // Valid alternative encodings share a decoded content address and
        // must not be compared with, or replaced by, the preferred encoding.
        let alternative = temporary();
        let bytes = encoded(&serde_json::to_vec(&observed).unwrap(), 7);
        fs::write(blob_path(&alternative.0, &reference), &bytes).unwrap();
        let written = put(&alternative.0, &observed, &mut Meter::default()).unwrap();
        assert_eq!(written, reference);
        assert_eq!(
            fs::read(blob_path(&alternative.0, &reference)).unwrap(),
            bytes
        );
        assert_eq!(
            read(&alternative.0, &reference, &mut Meter::default()).unwrap(),
            observed
        );
    }

    #[test]
    fn rejects_corruption_extra_frames_trailing_bytes_and_preserves_existing() {
        let observed = fixture();
        let raw = serde_json::to_vec(&observed).unwrap();
        let reference = descriptor(&raw, observed.len() as u64);
        let good = encoded(&raw, 1);
        let mut cases = Vec::new();
        cases.push(good[..good.len() - 1].to_vec());
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        cases.push(bad);
        let mut bad = good.clone();
        bad[4] &= !0x04;
        cases.push(bad);
        cases.push([good.as_slice(), good.as_slice()].concat());
        cases.push([good.as_slice(), b"trailing"].concat());
        cases.push(b"{}".to_vec());
        cases.push(vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0]);
        for bad in cases {
            let temp = temporary();
            let path = blob_path(&temp.0, &reference);
            fs::write(&path, &bad).unwrap();
            assert!(verify(&temp.0, &reference, &mut Meter::default()).is_err());
            assert!(read(&temp.0, &reference, &mut Meter::default()).is_err());
            assert!(put(&temp.0, &observed, &mut Meter::default()).is_err());
            assert_eq!(fs::read(&path).unwrap(), bad);
        }
        let temp = temporary();
        let mut wrong = reference.clone();
        wrong.hash = "0".repeat(64);
        fs::write(blob_path(&temp.0, &wrong), good).unwrap();
        assert!(verify(&temp.0, &wrong, &mut Meter::default()).is_err());
    }

    #[test]
    fn rejects_noncanonical_maps_fields_and_invalid_paths() {
        let observation = r#"{"id":"x","device":1,"inode":"00000000000000000000000000000001"}"#;
        let cases = [
            (format!("{{\"b\":{observation},\"a\":{observation}}}"), 2),
            (format!("{{\"a\":{observation},\"a\":{observation}}}"), 2),
            (format!("{{ \"a\":{observation}}}"), 1),
            (format!("{{\"a\":{observation}}}\n"), 1),
            (format!("{{\"\\u0061\":{observation}}}"), 1),
            (format!("{{\"../a\":{observation}}}"), 1),
            (format!("{{\"a//b\":{observation}}}"), 1),
            (r#"{"a":{"device":1,"id":"x","inode":"00000000000000000000000000000001"}}"#.to_owned(), 1),
            (r#"{"a":{"id":"x","link_group":null,"device":1,"inode":"00000000000000000000000000000001"}}"#.to_owned(), 1),
            (r#"{"a":{"id":"x","device":1,"inode":"00000000000000000000000000000001","extra":0}}"#.to_owned(), 1),
            (r#"{"a":{"id":"x","id":"y","device":1,"inode":"00000000000000000000000000000001"}}"#.to_owned(), 1),
            (r#"{"a":{"id":"x","device":1,"inode":"FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"}}"#.to_owned(), 1),
            (r#"{"a":{"id":"x","device":1,"inode":1}}"#.to_owned(), 1),
            (r#"{"a":{"id":"x","device":1,"inode":"1"}}"#.to_owned(), 1),
            ("[]".to_owned(), 0),
        ];
        for (raw, entries) in cases {
            let temp = temporary();
            let reference = store_raw(&temp.0, raw.as_bytes(), entries);
            assert!(
                verify(&temp.0, &reference, &mut Meter::default()).is_err(),
                "{raw}"
            );
        }
        let reference = empty_reference();
        let mut json = serde_json::to_value(&reference).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("unknown".into(), true.into());
        assert!(serde_json::from_value::<ObservationRef>(json).is_err());
    }

    #[test]
    fn budgets_charge_actual_distinct_refs_not_declared_lengths() {
        let temp = temporary();
        let observed = fixture();
        let first = put(&temp.0, &observed, &mut Meter::default()).unwrap();
        let mut second_map = observed.clone();
        second_map.get_mut("a").unwrap().device = 7;
        let second = put(&temp.0, &second_map, &mut Meter::default()).unwrap();
        let budget = ObservationBudget {
            max_raw_bytes: first.raw_bytes,
            max_entries: first.entries,
            ..ObservationBudget::default()
        };
        let mut meter = Meter::new(budget);
        verify(&temp.0, &first, &mut meter).unwrap();
        verify(&temp.0, &first, &mut meter).unwrap();
        assert_eq!(read(&temp.0, &first, &mut meter).unwrap(), observed);
        assert_eq!(put(&temp.0, &observed, &mut meter).unwrap(), first);
        assert_eq!(meter.raw_bytes(), first.raw_bytes);
        assert_eq!(meter.entries(), first.entries);
        assert!(verify(&temp.0, &second, &mut meter).is_err());

        for budget in [
            ObservationBudget {
                max_raw_bytes: first.raw_bytes - 1,
                max_entries: first.entries,
                ..ObservationBudget::default()
            },
            ObservationBudget {
                max_raw_bytes: first.raw_bytes,
                max_entries: first.entries - 1,
                ..ObservationBudget::default()
            },
        ] {
            assert!(read(&temp.0, &first, &mut Meter::new(budget)).is_err());
            assert!(put(&temp.0, &observed, &mut Meter::new(budget)).is_err());
        }
        let mut meter = Meter::new(ObservationBudget {
            max_raw_bytes: first.raw_bytes + second.raw_bytes,
            max_entries: first.entries + second.entries,
            ..ObservationBudget::default()
        });
        verify(&temp.0, &first, &mut meter).unwrap();
        verify(&temp.0, &second, &mut meter).unwrap();
        assert_eq!(meter.raw_bytes(), first.raw_bytes + second.raw_bytes);
        assert_eq!(meter.entries(), first.entries + second.entries);

        let mut dishonest = first.clone();
        dishonest.raw_bytes = u64::MAX;
        dishonest.entries = u64::MAX;
        let mut meter = Meter::default();
        assert!(read(&temp.0, &dishonest, &mut meter).is_err());
        assert_eq!(meter.raw_bytes(), first.raw_bytes);
        assert_eq!(meter.entries(), first.entries);
        let mut dishonest = first.clone();
        dishonest.entries += 1;
        assert!(verify(&temp.0, &dishonest, &mut Meter::default()).is_err());
        let mut dishonest = first.clone();
        dishonest.raw_bytes -= 1;
        assert!(verify(&temp.0, &dishonest, &mut Meter::default()).is_err());
    }

    #[test]
    fn verification_cache_is_not_shared_between_locations_or_descriptors() {
        let first = temporary();
        let second = temporary();
        let observed = fixture();
        let reference = put(&first.0, &observed, &mut Meter::default()).unwrap();
        let mut meter = Meter::default();
        verify(&first.0, &reference, &mut meter).unwrap();
        assert!(verify(&second.0, &reference, &mut meter).is_err());
        let mut inconsistent = reference.clone();
        inconsistent.entries -= 1;
        assert!(verify(&first.0, &inconsistent, &mut meter).is_err());
        assert!(read(&first.0, &inconsistent, &mut meter).is_err());
        let mut invalid = reference;
        invalid.hash = "../escape".to_owned();
        assert!(validate_descriptor(&invalid).is_err());
        assert!(read(&first.0, &invalid, &mut Meter::default()).is_err());
    }

    #[test]
    fn orphan_inspection_and_encoded_work_are_bounded_and_configurable() {
        let temp = temporary();
        let observed = fixture();
        let reference = put(&temp.0, &observed, &mut Meter::default()).unwrap();
        let length = fs::metadata(blob_path(&temp.0, &reference)).unwrap().len();
        let mut meter = Meter::new(ObservationBudget {
            max_encoded_bytes: length,
            ..ObservationBudget::default()
        });
        assert_eq!(
            inspect(&temp.0, &reference.hash, &mut meter).unwrap(),
            reference
        );
        assert_eq!(meter.encoded_bytes(), length);
        assert_eq!(meter.raw_bytes(), reference.raw_bytes);
        verify(&temp.0, &reference, &mut meter).unwrap();
        assert_eq!(
            inspect(&temp.0, &reference.hash, &mut meter).unwrap(),
            reference
        );
        assert_eq!(meter.encoded_bytes(), length);
        // Returning another owned map actually reads compressed bytes again.
        assert!(read(&temp.0, &reference, &mut meter).is_err());
        assert_eq!(meter.encoded_bytes(), length + 1);

        let mut meter = Meter::new(ObservationBudget {
            max_encoded_bytes: length * 2,
            ..ObservationBudget::default()
        });
        verify(&temp.0, &reference, &mut meter).unwrap();
        assert_eq!(read(&temp.0, &reference, &mut meter).unwrap(), observed);
        assert_eq!(meter.encoded_bytes(), length * 2);
        assert_eq!(meter.raw_bytes(), reference.raw_bytes);

        let mut meter = Meter::new(ObservationBudget {
            max_encoded_bytes: 0,
            ..ObservationBudget::default()
        });
        let output = temp.0.join("new");
        assert!(put(&output, &observed, &mut meter).is_err());
        assert_eq!(meter.encoded_bytes(), 0);
        assert_eq!(fs::read_dir(&output).unwrap().count(), 0);

        // Ordinary zstd frames may contain empty raw blocks. This is a valid
        // alternative representation, not an encoded/raw ratio format error.
        // Enough empty blocks still consume a configurable IO-work budget.
        let raw = b"{}";
        let original = encoded(raw, 1);
        let mut empty_blocks = vec![0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x50];
        empty_blocks.resize(empty_blocks.len() + 3 * 5000, 0);
        empty_blocks.extend_from_slice(&[0x11, 0, 0]); // Last raw block, two bytes.
        empty_blocks.extend_from_slice(raw);
        empty_blocks.extend_from_slice(&original[original.len() - 4..]);
        let reference = descriptor(raw, 0);
        fs::write(blob_path(&temp.0, &reference), &empty_blocks).unwrap();
        let mut meter = Meter::new(ObservationBudget {
            max_raw_bytes: 2,
            max_entries: 0,
            max_encoded_bytes: 9000,
        });
        assert!(inspect(&temp.0, &reference.hash, &mut meter).is_err());
        assert_eq!(meter.encoded_bytes(), 9001);
        assert_eq!(meter.raw_bytes(), 0);
        let mut meter = Meter::new(ObservationBudget {
            max_raw_bytes: 2,
            max_entries: 0,
            max_encoded_bytes: empty_blocks.len() as u64,
        });
        assert_eq!(
            inspect(&temp.0, &reference.hash, &mut meter).unwrap(),
            reference
        );
        assert_eq!(meter.encoded_bytes(), empty_blocks.len() as u64);
    }

    #[test]
    fn rejects_oversized_codec_window_and_unsafe_leaf_types() {
        let temp = temporary();
        let reference = empty_reference();
        let original = encoded(b"{}", 1);
        // A normal non-single-segment frame with a 2 MiB window is beyond
        // this codec's fixed memory contract, regardless of its tiny payload.
        let mut large_window = vec![0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58];
        large_window.extend_from_slice(&[0x11, 0, 0]);
        large_window.extend_from_slice(b"{}");
        large_window.extend_from_slice(&original[original.len() - 4..]);
        let path = blob_path(&temp.0, &reference);
        fs::write(&path, large_window).unwrap();
        assert!(verify(&temp.0, &reference, &mut Meter::default()).is_err());
        fs::remove_file(&path).unwrap();
        platform::create_dir(&path, 0o700).unwrap();
        assert!(read(&temp.0, &reference, &mut Meter::default()).is_err());
        assert!(put(&temp.0, &Observed::new(), &mut Meter::default()).is_err());

        #[cfg(unix)]
        {
            fs::remove_dir(&path).unwrap();
            let target = temp.0.join("elsewhere");
            fs::write(&target, original).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(read(&temp.0, &reference, &mut Meter::default()).is_err());
            assert!(put(&temp.0, &Observed::new(), &mut Meter::default()).is_err());
            assert!(
                fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn publication_faults_preserve_only_safe_orphans_and_retry_verifies_them() {
        let observed = fixture();
        let expected = descriptor(
            &serde_json::to_vec(&observed).unwrap(),
            observed.len() as u64,
        );
        for phase in [
            PublicationPhase::TemporaryWritten,
            PublicationPhase::TemporarySynced,
            PublicationPhase::Published,
            PublicationPhase::BeforeDirectorySync,
        ] {
            let temp = temporary();
            set_publication_fault(Some(phase));
            let result = put(&temp.0, &observed, &mut Meter::default());
            set_publication_fault(None);
            assert!(result.is_err(), "{phase:?}");
            let published = matches!(
                phase,
                PublicationPhase::Published | PublicationPhase::BeforeDirectorySync
            );
            assert_eq!(
                blob_path(&temp.0, &expected).exists(),
                published,
                "{phase:?}"
            );
            assert_eq!(
                fs::read_dir(&temp.0).unwrap().count(),
                usize::from(published)
            );
            if published {
                verify(&temp.0, &expected, &mut Meter::default()).unwrap();
            }
            assert_eq!(
                put(&temp.0, &observed, &mut Meter::default()).unwrap(),
                expected
            );
            assert_eq!(
                read(&temp.0, &expected, &mut Meter::default()).unwrap(),
                observed
            );
        }
        let temp = temporary();
        put(&temp.0, &observed, &mut Meter::default()).unwrap();
        let before = fs::read(blob_path(&temp.0, &expected)).unwrap();
        set_publication_fault(Some(PublicationPhase::DedupBeforeSync));
        let result = put(&temp.0, &observed, &mut Meter::default());
        set_publication_fault(None);
        assert!(result.is_err());
        assert_eq!(fs::read(blob_path(&temp.0, &expected)).unwrap(), before);
        assert_eq!(
            put(&temp.0, &observed, &mut Meter::default()).unwrap(),
            expected
        );
    }
}
