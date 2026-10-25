//! Shared immutable pages for the canonical, sorted flat manifest JSON stream.
//!
//! This is a bounded representation, not a second manifest model. Path-only
//! entry groups keep value edits from moving later groups. Physical pages align
//! with entries except when a single oversized entry must span several pages.
//! Reads reassemble and validate the exact canonical bytes before returning the
//! ordinary public `Manifest`. A writer finishes its one
//! object batch before returning any usable root; the caller still owns durable
//! root/revision publication. Readers reuse one caller-owned packed snapshot.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, BufRead, Cursor, Read, Write};
use std::path::Path;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::content::{self, Entry, EntryKind, Manifest, ObjectBatch};
use crate::{Result, metadata, packed};

const MIN_GROUP_ENTRIES: usize = 64;
const ANCHOR_PERIOD_ENTRIES: usize = 128;
const MAX_GROUP_ENTRIES: usize = 256;
const MAX_PAGE_BYTES: usize = 256 * 1024;
// Bound decoded metadata and references. Never silently truncate a larger manifest.
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
// At most ceil(N / MIN_GROUP_ENTRIES) logical groups. Within each group,
// ordinary greedy entry-aligned runs use at most 2 * raw_bytes / MAX_PAGE_BYTES
// pages plus one per run: every adjacent pair exceeds MAX_PAGE_BYTES. Isolating
// each oversized entry adds one run and at most ceil(entry_bytes / MAX_PAGE_BYTES)
// fragments; since each such entry exceeds MAX_PAGE_BYTES, the total is bounded
// by groups + 3 * raw_bytes / MAX_PAGE_BYTES. Small group ends and isolated
// remainders are valid non-final pages, so the old 16 KiB minimum cannot apply.
const MAX_PAGES: usize =
    MAX_ENTRIES.div_ceil(MIN_GROUP_ENTRIES) + 3 * MAX_MANIFEST_BYTES.div_ceil(MAX_PAGE_BYTES);
const MAX_ENCODED_PAGE_BYTES: usize = 512 * 1024;
// Format envelope for the locked metadata encoder: zstd 1.5.7 level 1,
// pledged size, 1 MiB window, no intermediate flush, <=256 KiB input.
// Pre-split pieces are >=8 KiB. Nonexpanding blocks
// (at most 32, each 3-byte header), frame header <=18, checksum 4 and final
// empty block <=3 give N+121. Keep ample margin, not a claim about arbitrary
// streaming zstd encoders. Revisit when encoder settings/dependency change.
const MAX_PAGE_ENCODING_OVERHEAD: usize = 4096;
const MAX_TOTAL_ENCODED_BYTES: u64 =
    (MAX_MANIFEST_BYTES + MAX_PAGES * MAX_PAGE_ENCODING_OVERHEAD) as u64;
// Encoded objects still use the ordinary content codec's 16 KiB minimum chunk.
const MIN_OBJECT_CHUNK_BYTES: usize = 16 * 1024;
const MAX_PAGE_CHUNKS: usize = MAX_ENCODED_PAGE_BYTES / MIN_OBJECT_CHUNK_BYTES + 1;
const MANIFEST_DOMAIN: &[u8] = b"sprig-manifest-path-pages-v2\0";
const PAGE_DOMAIN: &[u8] = b"sprig-manifest-path-json-page-v2\0";
const ANCHOR_DOMAIN: &[u8] = b"sprig-manifest-path-anchor-v1\0";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ManifestFormat {
    #[serde(rename = "sprig_manifest_path_pages_v2")]
    FlatJsonPathZstdV2,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum PageFormat {
    #[serde(rename = "sprig_manifest_path_json_page_v2")]
    JsonFragmentZstdV2,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredManifest {
    format: ManifestFormat,
    raw_bytes: u64,
    #[serde(deserialize_with = "deserialize_hash")]
    hash: String,
    #[serde(deserialize_with = "deserialize_pages")]
    pages: Vec<StoredPage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredPage {
    format: PageFormat,
    raw_bytes: u32,
    #[serde(deserialize_with = "deserialize_hash")]
    hash: String,
    // Keep the existing physical-reference type; do not invent a free mapping
    // from semantic page hashes to encoded objects.
    #[serde(deserialize_with = "deserialize_encoded")]
    encoded: EntryKind,
}

fn invalid(message: impl Into<String>) -> crate::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}

fn digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(domain);
    hash.update(bytes);
    hash.finalize().to_hex().to_string()
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct HashValue(String);
impl<'de> Deserialize<'de> for HashValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct HashVisitor;
        impl Visitor<'_> for HashVisitor {
            type Value = HashValue;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical lowercase 64-character BLAKE3 hash")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
                if !valid_hash(value) {
                    return Err(E::custom("invalid manifest page hash"));
                }
                Ok(HashValue(value.to_owned()))
            }
        }
        deserializer.deserialize_str(HashVisitor)
    }
}

fn deserialize_hash<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(HashValue::deserialize(deserializer)?.0)
}

fn deserialize_chunks<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    struct ChunksVisitor;
    impl<'de> Visitor<'de> for ChunksVisitor {
        type Value = Vec<String>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded list of encoded-page chunk hashes")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut chunks = Vec::new();
            while let Some(hash) = seq.next_element::<HashValue>()? {
                if chunks.len() == MAX_PAGE_CHUNKS {
                    return Err(de::Error::custom("too many encoded-page chunks"));
                }
                chunks.push(hash.0);
            }
            Ok(chunks)
        }
    }
    deserializer.deserialize_seq(ChunksVisitor)
}

fn deserialize_encoded<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<EntryKind, D::Error> {
    #[derive(Deserialize)]
    enum FileTag {
        #[serde(rename = "file")]
        File,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EncodedFile {
        #[serde(rename = "type")]
        _kind: FileTag,
        size: u64,
        #[serde(deserialize_with = "deserialize_chunks")]
        chunks: Vec<String>,
    }
    let file = EncodedFile::deserialize(deserializer)?;
    if file.size == 0
        || file.size > MAX_ENCODED_PAGE_BYTES as u64
        || file.chunks.is_empty()
        || file.chunks.len() as u64 > file.size
    {
        return Err(de::Error::custom(
            "invalid encoded-page size or chunk count",
        ));
    }
    Ok(EntryKind::File {
        size: file.size,
        chunks: file.chunks,
    })
}

fn deserialize_pages<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<StoredPage>, D::Error> {
    struct PagesVisitor;
    impl<'de> Visitor<'de> for PagesVisitor {
        type Value = Vec<StoredPage>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded ordered list of manifest pages")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut pages = Vec::new();
            while let Some(page) = seq.next_element::<StoredPage>()? {
                if pages.len() == MAX_PAGES {
                    return Err(de::Error::custom("too many manifest pages"));
                }
                pages.push(page);
            }
            Ok(pages)
        }
    }
    deserializer.deserialize_seq(PagesVisitor)
}

struct LimitedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "manifest JSON exceeds the representation size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Both bounded sinks already track accepted bytes. Reuse that position rather
// than incrementing a second counter for every serde fragment. Callers start
// each canonical emission with an empty/zero-position sink.
trait PositionedWrite: Write {
    fn position(&self) -> usize;
}
impl PositionedWrite for LimitedBytes {
    fn position(&self) -> usize {
        self.bytes.len()
    }
}

/// Identical to serde_json's canonical BTreeMap emission. The first span owns
/// the opening brace, later spans their leading commas, and the last span the
/// closing brace. Every offset is recorded in the same bounded serialization.
fn emit_canonical(manifest: &Manifest, writer: &mut impl PositionedWrite) -> Result<Vec<usize>> {
    let mut ends = Vec::with_capacity(manifest.len());
    writer.write_all(b"{")?;
    for (number, (path, entry)) in manifest.iter().enumerate() {
        if number != 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, path)?;
        writer.write_all(b":")?;
        serde_json::to_writer(&mut *writer, entry)?;
        ends.push(writer.position());
    }
    writer.write_all(b"}")?;
    if let Some(last) = ends.last_mut() {
        *last = writer.position();
    }
    Ok(ends)
}

fn path_anchor(path: &str) -> bool {
    let mut hash = blake3::Hasher::new();
    hash.update(ANCHOR_DOMAIN);
    hash.update(&(path.len() as u64).to_le_bytes());
    hash.update(path.as_bytes());
    hash.finalize().as_bytes()[0] & (ANCHOR_PERIOD_ENTRIES - 1) as u8 == 0
}

fn finish_page(lengths: &mut Vec<usize>, pending: &mut usize) -> Result<()> {
    if *pending != 0 {
        if *pending > MAX_PAGE_BYTES || lengths.len() == MAX_PAGES {
            return Err(invalid("manifest exceeds the page representation limit"));
        }
        lengths.push(*pending);
        *pending = 0;
    }
    Ok(())
}

/// Path-only logical groups are not stored as another tree. Byte-cap cuts stay
/// inside a group and never reset its path schedule. Entry sizes can therefore
/// affect only the physical pages of their own group, not all following groups.
fn page_lengths(manifest: &Manifest, ends: &[usize]) -> Result<Vec<usize>> {
    if manifest.is_empty() || manifest.len() > MAX_ENTRIES || ends.len() != manifest.len() {
        return Err(invalid("invalid manifest entry spans"));
    }
    let mut lengths = Vec::new();
    let mut previous = 0;
    let mut pending = 0;
    let mut group_entries = 0;
    for (path, &end) in manifest.keys().zip(ends) {
        if end <= previous || end > MAX_MANIFEST_BYTES {
            return Err(invalid("invalid manifest entry byte offset"));
        }
        let length = end - previous;
        previous = end;
        if length > MAX_PAGE_BYTES {
            finish_page(&mut lengths, &mut pending)?;
            let mut remaining = length;
            while remaining != 0 {
                pending = remaining.min(MAX_PAGE_BYTES);
                remaining -= pending;
                finish_page(&mut lengths, &mut pending)?;
            }
        } else {
            if pending + length > MAX_PAGE_BYTES {
                finish_page(&mut lengths, &mut pending)?;
            }
            pending += length;
        }
        group_entries += 1;
        if group_entries == MAX_GROUP_ENTRIES
            || (group_entries >= MIN_GROUP_ENTRIES && path_anchor(path))
        {
            finish_page(&mut lengths, &mut pending)?;
            group_entries = 0;
        }
    }
    finish_page(&mut lengths, &mut pending)?;
    Ok(lengths)
}

/// Store compressed path-group pages and finish the single durability batch
/// before returning their root. The caller must then durably publish that root.
pub(crate) fn write(manifest: &Manifest, objects: &Path) -> Result<StoredManifest> {
    if manifest.len() > MAX_ENTRIES {
        return Err(invalid("manifest exceeds the entry-count limit"));
    }
    content::validate_manifest(manifest)?;
    let mut output = LimitedBytes {
        bytes: Vec::new(),
        limit: MAX_MANIFEST_BYTES,
    };
    let ends = emit_canonical(manifest, &mut output)?;
    let lengths = page_lengths(manifest, &ends)?;
    let raw = output.bytes;
    let mut root = StoredManifest {
        format: ManifestFormat::FlatJsonPathZstdV2,
        raw_bytes: raw.len() as u64,
        hash: digest(MANIFEST_DOMAIN, &raw),
        pages: Vec::new(),
    };
    let mut batch = ObjectBatch::open(objects)?;
    let mut remaining = raw.as_slice();
    for length in lengths {
        let bytes = &remaining[..length];
        let encoded = metadata::encode(bytes)?;
        if encoded.len() > MAX_ENCODED_PAGE_BYTES
            || encoded.len() > length + MAX_PAGE_ENCODING_OVERHEAD
            || root.pages.len() == MAX_PAGES
        {
            return Err(invalid("manifest exceeds the page representation limit"));
        }
        root.pages.push(StoredPage {
            format: PageFormat::JsonFragmentZstdV2,
            raw_bytes: length as u32,
            hash: digest(PAGE_DOMAIN, bytes),
            encoded: batch.put_reader(&mut Cursor::new(encoded))?,
        });
        remaining = &remaining[length..];
    }
    if !remaining.is_empty() {
        return Err(invalid("manifest pages do not cover the canonical bytes"));
    }
    validate_root(&root)?;
    batch.finish()?;
    Ok(root)
}

fn validate_root(root: &StoredManifest) -> Result<()> {
    if root.raw_bytes == 0
        || root.raw_bytes > MAX_MANIFEST_BYTES as u64
        || root.pages.is_empty()
        || root.pages.len() > MAX_PAGES
        || !valid_hash(&root.hash)
    {
        return Err(invalid("invalid manifest root bounds or hash"));
    }
    let mut total = 0_u64;
    let mut encoded_total = 0_u64;
    for page in &root.pages {
        let length = page.raw_bytes as usize;
        if length == 0 || length > MAX_PAGE_BYTES || !valid_hash(&page.hash) {
            return Err(invalid("invalid manifest page bounds or hash"));
        }
        let EntryKind::File { size, chunks } = &page.encoded else {
            return Err(invalid("manifest page must have file storage"));
        };
        if *size == 0
            || *size > MAX_ENCODED_PAGE_BYTES as u64
            || *size > page.raw_bytes as u64 + MAX_PAGE_ENCODING_OVERHEAD as u64
            || chunks.is_empty()
            || chunks.len() > MAX_PAGE_CHUNKS
            || chunks.len() as u64 > *size
            || *size > (chunks.len() as u64) * MAX_PAGE_BYTES as u64
            || !chunks.iter().all(|hash| valid_hash(hash))
        {
            return Err(invalid("invalid encoded manifest page storage"));
        }
        encoded_total = encoded_total
            .checked_add(*size)
            .filter(|total| *total <= MAX_TOTAL_ENCODED_BYTES)
            .ok_or_else(|| invalid("manifest encoded byte budget exceeded"))?;
        total = total
            .checked_add(page.raw_bytes as u64)
            .ok_or_else(|| invalid("manifest page length overflow"))?;
    }
    if total != root.raw_bytes {
        return Err(invalid("manifest page lengths do not match the root"));
    }
    Ok(())
}

fn read_page(
    page: &StoredPage,
    read_object: &mut impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let EntryKind::File { size, chunks } = &page.encoded else {
        return Err(invalid("manifest page must have file storage"));
    };
    let mut encoded = Vec::with_capacity(*size as usize);
    for hash in chunks {
        let bytes = read_object(hash)?;
        if bytes.len() > (*size as usize).saturating_sub(encoded.len()) {
            return Err(invalid("encoded-page chunks exceed the declared size"));
        }
        encoded.extend(bytes);
    }
    if encoded.len() as u64 != *size {
        return Err(invalid(
            "encoded-page chunks do not match the declared size",
        ));
    }
    // Require an ordinary checksummed zstd frame, not a skippable frame. The
    // decoder also enforces the 1 MiB codec window and validates the checksum.
    if encoded.len() < 5 || encoded[..4] != [0x28, 0xb5, 0x2f, 0xfd] || encoded[4] & 0x04 == 0 {
        return Err(invalid("manifest page requires a checksummed zstd frame"));
    }
    let mut decoder =
        zstd::stream::read::Decoder::with_buffer(Cursor::new(encoded))?.single_frame();
    decoder.window_log_max(20)?;
    let mut raw = Vec::with_capacity(page.raw_bytes as usize);
    decoder
        .by_ref()
        .take(page.raw_bytes as u64 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() != page.raw_bytes as usize {
        return Err(invalid("decoded manifest page length mismatch"));
    }
    if !decoder.finish().fill_buf()?.is_empty() {
        return Err(invalid("trailing bytes or frames after a manifest page"));
    }
    if digest(PAGE_DOMAIN, &raw) != page.hash {
        return Err(invalid("manifest page hash mismatch"));
    }
    Ok(raw)
}

struct OrderedManifestVisitor {
    entry_limit: usize,
}

impl<'de> Visitor<'de> for OrderedManifestVisitor {
    type Value = Manifest;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical strictly sorted manifest map")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Manifest, A::Error> {
        let mut manifest = Manifest::new();
        while let Some(path) = map.next_key::<String>()? {
            if manifest.len() == self.entry_limit {
                return Err(de::Error::custom("manifest exceeds the entry-count limit"));
            }
            if manifest
                .last_key_value()
                .is_some_and(|(previous, _)| previous >= &path)
            {
                return Err(de::Error::custom(
                    "manifest keys must be strictly sorted and unique",
                ));
            }
            let entry = map.next_value::<Entry>()?;
            manifest.insert(path, entry);
        }
        Ok(manifest)
    }
}

struct CanonicalCheck<'a> {
    expected: &'a [u8],
    offset: usize,
}
impl PositionedWrite for CanonicalCheck<'_> {
    fn position(&self) -> usize {
        self.offset
    }
}
impl Write for CanonicalCheck<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = &self.expected[self.offset..];
        if !remaining.starts_with(bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "noncanonical manifest JSON",
            ));
        }
        self.offset += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn parse_canonical(raw: &[u8], entry_limit: usize) -> Result<(Manifest, Vec<usize>)> {
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let manifest =
        Deserializer::deserialize_map(&mut decoder, OrderedManifestVisitor { entry_limit })?;
    decoder.end()?;
    content::validate_manifest(&manifest)?;
    let mut check = CanonicalCheck {
        expected: raw,
        offset: 0,
    };
    let ends = emit_canonical(&manifest, &mut check)?;
    if check.offset != raw.len() {
        return Err(invalid("trailing noncanonical manifest JSON"));
    }
    Ok((manifest, ends))
}

/// Flatten pages through a single validated caller-owned index snapshot.
pub(crate) fn read(root: &StoredManifest, reader: &packed::Reader) -> Result<Manifest> {
    read_with(root, &mut |hash| reader.read(hash))
}

/// Ordinary reads verify every requested metadata payload without auditing
/// unrelated index records. A single pinned point reader spans all pages.
pub(crate) fn read_point(
    root: &StoredManifest,
    reader: &mut packed::PointReader,
) -> Result<Manifest> {
    read_with(root, &mut |hash| reader.read(hash))
}

fn read_with(
    root: &StoredManifest,
    read_object: &mut impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<Manifest> {
    validate_root(root)?;
    let mut raw = Vec::with_capacity(root.raw_bytes as usize);
    for page in &root.pages {
        raw.extend(read_page(page, read_object)?);
    }
    if raw.len() as u64 != root.raw_bytes || digest(MANIFEST_DOMAIN, &raw) != root.hash {
        return Err(invalid("manifest byte length or canonical digest mismatch"));
    }
    let (manifest, ends) = parse_canonical(&raw, MAX_ENTRIES)?;
    let lengths = page_lengths(&manifest, &ends)?;
    if !lengths
        .iter()
        .copied()
        .eq(root.pages.iter().map(|page| page.raw_bytes as usize))
    {
        return Err(invalid("noncanonical manifest path-group page boundaries"));
    }
    Ok(manifest)
}

/// Metadata object chunks only. File-payload chunks remain in the flattened
/// Manifest and must also be included by inventory/GC callers.
pub(crate) fn referenced_chunks(root: &StoredManifest) -> BTreeSet<String> {
    root.pages
        .iter()
        .flat_map(|page| match &page.encoded {
            EntryKind::File { chunks, .. } => chunks.as_slice(),
            _ => &[],
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NONCE: AtomicU64 = AtomicU64::new(0);
    struct TestDir(std::path::PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn temp_dir() -> TestDir {
        let path = std::env::temp_dir().join(format!(
            "sprig-manifest-pages-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        TestDir(fs::canonicalize(path).unwrap())
    }

    fn planned(manifest: &Manifest) -> (Vec<u8>, Vec<usize>, Vec<usize>) {
        let mut output = LimitedBytes {
            bytes: Vec::new(),
            limit: MAX_MANIFEST_BYTES,
        };
        let ends = emit_canonical(manifest, &mut output).unwrap();
        let lengths = page_lengths(manifest, &ends).unwrap();
        assert_eq!(output.bytes, serde_json::to_vec(manifest).unwrap());
        assert_eq!(lengths.iter().sum::<usize>(), output.bytes.len());
        (output.bytes, ends, lengths)
    }

    fn split_root(raw: &[u8], lengths: &[usize], objects: &Path) -> StoredManifest {
        assert_eq!(lengths.iter().sum::<usize>(), raw.len());
        let mut root = StoredManifest {
            format: ManifestFormat::FlatJsonPathZstdV2,
            raw_bytes: raw.len() as u64,
            hash: digest(MANIFEST_DOMAIN, raw),
            pages: Vec::new(),
        };
        let mut batch = ObjectBatch::open(objects).unwrap();
        let mut offset = 0;
        for &length in lengths {
            let bytes = &raw[offset..offset + length];
            root.pages.push(StoredPage {
                format: PageFormat::JsonFragmentZstdV2,
                raw_bytes: length as u32,
                hash: digest(PAGE_DOMAIN, bytes),
                encoded: batch
                    .put_reader(&mut Cursor::new(metadata::encode(bytes).unwrap()))
                    .unwrap(),
            });
            offset += length;
        }
        batch.finish().unwrap();
        root
    }

    fn fixture() -> Manifest {
        let mut manifest = Manifest::new();
        manifest.insert(
            "".into(),
            Entry {
                id: "root".into(),
                link_group: None,
                mode: 0o755,
                modified_ns: -1,
                kind: EntryKind::Directory,
            },
        );
        for number in 0..2000 {
            manifest.insert(
                format!("file-{number:04}"),
                Entry {
                    id: format!("stable-{number:04}"),
                    link_group: None,
                    mode: 0o644,
                    modified_ns: i64::MAX as i128 + 1,
                    kind: EntryKind::File {
                        size: 1,
                        chunks: vec![
                            blake3::hash(format!("payload-{number}").as_bytes())
                                .to_hex()
                                .to_string(),
                        ],
                    },
                },
            );
        }
        manifest.insert(
            "symlink".into(),
            Entry {
                id: "link".into(),
                link_group: None,
                mode: 0o777,
                modified_ns: 0,
                kind: EntryKind::Symlink {
                    target: vec![b'a', 255, b'b'],
                },
            },
        );
        manifest
    }
    fn raw_root(raw: &[u8], objects: &Path, encoded: Option<Vec<u8>>) -> StoredManifest {
        let encoded = encoded.unwrap_or_else(|| metadata::encode(raw).unwrap());
        let mut batch = ObjectBatch::open(objects).unwrap();
        let storage = batch.put_reader(&mut Cursor::new(encoded)).unwrap();
        batch.finish().unwrap();
        StoredManifest {
            format: ManifestFormat::FlatJsonPathZstdV2,
            raw_bytes: raw.len() as u64,
            hash: digest(MANIFEST_DOMAIN, raw),
            pages: vec![StoredPage {
                format: PageFormat::JsonFragmentZstdV2,
                raw_bytes: raw.len() as u32,
                hash: digest(PAGE_DOMAIN, raw),
                encoded: storage,
            }],
        }
    }

    #[test]
    fn roundtrip_corruption_and_limits() {
        let temp = temp_dir();
        let objects = temp.0.join("objects");
        let original = fixture();
        let root = write(&original, &objects).unwrap();
        assert!(root.pages.len() > 1);
        let encoded_root = serde_json::to_vec(&root).unwrap();
        let decoded_root: StoredManifest = serde_json::from_slice(&encoded_root).unwrap();
        assert_eq!(root, decoded_root);
        let reader = packed::Reader::open(&objects).unwrap();
        assert_eq!(read(&decoded_root, &reader).unwrap(), original);
        let mut point = packed::PointReader::open(&objects).unwrap();
        assert_eq!(read_point(&decoded_root, &mut point).unwrap(), original);
        assert_eq!(write(&original, &objects).unwrap(), root);
        let mut edited = original.clone();
        edited.get_mut("file-1999").unwrap().mode = 0o755;
        let next = write(&edited, &objects).unwrap();
        assert_ne!(next.hash, root.hash);
        assert!(!referenced_chunks(&root).is_disjoint(&referenced_chunks(&next)));
        assert_eq!(
            read(&next, &packed::Reader::open(&objects).unwrap()).unwrap(),
            edited
        );

        for case in 0..7 {
            let mut bad = root.clone();
            match case {
                0 => bad.hash = "0".repeat(64),
                1 => bad.pages[0].hash = "0".repeat(64),
                2 => bad.raw_bytes = MAX_MANIFEST_BYTES as u64 + 1,
                3 => bad.pages[0].raw_bytes = MAX_PAGE_BYTES as u32 + 1,
                4 => bad.pages.reverse(),
                5 => bad.pages = vec![root.pages[0].clone(); MAX_PAGES + 1],
                6 => bad.pages[0].encoded = EntryKind::Directory,
                _ => unreachable!(),
            }
            assert!(read(&bad, &reader).is_err(), "case {case}");
        }
        assert!(
            serde_json::from_slice::<StoredManifest>(
                &encoded_root
                    .iter()
                    .copied()
                    .take(encoded_root.len() - 1)
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
        for (current, old) in [
            ("sprig_manifest_path_pages_v2", "sprig_manifest_pages_v1"),
            (
                "sprig_manifest_path_json_page_v2",
                "sprig_manifest_json_page_v1",
            ),
        ] {
            let wrong_format = String::from_utf8(encoded_root.clone())
                .unwrap()
                .replace(current, old);
            assert!(serde_json::from_str::<StoredManifest>(&wrong_format).is_err());
        }
        let mut wrong_domain = root.clone();
        wrong_domain.hash = digest(
            b"sprig-manifest-pages-v1\0",
            &serde_json::to_vec(&original).unwrap(),
        );
        assert!(read(&wrong_domain, &reader).is_err());
        let mut too_many = root.clone();
        too_many.pages = vec![root.pages[0].clone(); MAX_PAGES + 1];
        assert!(
            serde_json::from_slice::<StoredManifest>(&serde_json::to_vec(&too_many).unwrap())
                .is_err()
        );
        let mut too_many_chunks = root.clone();
        too_many_chunks.pages[0].encoded = EntryKind::File {
            size: MAX_ENCODED_PAGE_BYTES as u64,
            chunks: vec!["0".repeat(64); MAX_PAGE_CHUNKS + 1],
        };
        assert!(
            serde_json::from_slice::<StoredManifest>(
                &serde_json::to_vec(&too_many_chunks).unwrap()
            )
            .is_err()
        );
        let mut limited = LimitedBytes {
            bytes: Vec::new(),
            limit: 4,
        };
        assert!(limited.write_all(b"12345").is_err());

        let mut tiny = Manifest::new();
        tiny.insert("".into(), original[""].clone());
        let raw = serde_json::to_vec(&tiny).unwrap();
        assert!(parse_canonical(&raw, 0).is_err());
        let duplicate = format!(
            "{{\"\":{},\"\":{}}}",
            serde_json::to_string(&original[""]).unwrap(),
            serde_json::to_string(&original[""]).unwrap()
        );
        let whitespace = format!(" {}", String::from_utf8(raw.clone()).unwrap());
        let out_of_order = format!(
            "{{\"file-0000\":{},\"\":{}}}",
            serde_json::to_string(&original["file-0000"]).unwrap(),
            serde_json::to_string(&original[""]).unwrap()
        );
        for malformed in [
            duplicate.as_bytes(),
            whitespace.as_bytes(),
            out_of_order.as_bytes(),
        ] {
            let bad = raw_root(malformed, &objects, None);
            assert!(read(&bad, &packed::Reader::open(&objects).unwrap()).is_err());
        }
        let frame = metadata::encode(&raw).unwrap();
        let mut corrupt = frame.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        let mut trailing = frame.clone();
        trailing.extend(&frame);
        let plain = raw.clone();
        for bytes in [frame[..frame.len() - 1].to_vec(), corrupt, trailing, plain] {
            let bad = raw_root(&raw, &objects, Some(bytes));
            assert!(read(&bad, &packed::Reader::open(&objects).unwrap()).is_err());
        }
    }

    #[test]
    fn path_groups_ignore_id_seeds_and_value_edits() {
        let temp = temp_dir();
        let objects = temp.0.join("objects");
        let original: Manifest = fixture().into_iter().take(334).collect();
        let mut expected_lengths = None;
        for seed in 0_u64..8 {
            let mut manifest = original.clone();
            for (path, entry) in &mut manifest {
                let mut hash = blake3::Hasher::new();
                hash.update(b"sprig-path-page-test-identity\0");
                hash.update(&seed.to_le_bytes());
                hash.update(path.as_bytes());
                entry.id = hash.finalize().to_hex().to_string();
            }
            let (raw, ends, lengths) = planned(&manifest);
            assert!(lengths.len() >= 2);
            if let Some(expected) = &expected_lengths {
                assert_eq!(&lengths, expected);
            } else {
                expected_lengths = Some(lengths.clone());
            }
            let (parsed, parsed_ends) = parse_canonical(&raw, MAX_ENTRIES).unwrap();
            assert_eq!(parsed, manifest);
            assert_eq!(parsed_ends, ends);
            let before = write(&manifest, &objects).unwrap();
            assert_eq!(write(&manifest, &objects).unwrap(), before);
            assert_eq!(
                read(&before, &packed::Reader::open(&objects).unwrap()).unwrap(),
                manifest
            );

            let entry = manifest.get_mut("file-0000").unwrap();
            let EntryKind::File { chunks, .. } = &mut entry.kind else {
                unreachable!();
            };
            chunks[0] = blake3::hash(b"edited payload").to_hex().to_string();
            let (_, _, edited_lengths) = planned(&manifest);
            assert_eq!(edited_lengths, lengths);
            let after = write(&manifest, &objects).unwrap();
            assert_ne!(before.pages[0].hash, after.pages[0].hash);
            assert_eq!(&before.pages[1..], &after.pages[1..]);
            assert_eq!(
                read(&after, &packed::Reader::open(&objects).unwrap()).unwrap(),
                manifest
            );

            // A value's encoded length can change without moving later groups.
            manifest
                .get_mut("file-0000")
                .unwrap()
                .id
                .push_str("-longer");
            let longer = write(&manifest, &objects).unwrap();
            assert_eq!(&after.pages[1..], &longer.pages[1..]);
        }
    }

    #[test]
    fn canonical_emission_insert_delete_rename_and_wrong_partitions() {
        let temp = temp_dir();
        let objects = temp.0.join("objects");
        let original = fixture();
        let before = write(&original, &objects).unwrap();
        for operation in 0..3 {
            let mut changed = original.clone();
            match operation {
                0 => {
                    let mut entry = changed["file-1000"].clone();
                    entry.id = "inserted-identity".into();
                    changed.insert("file-1000-new".into(), entry);
                }
                1 => {
                    changed.remove("file-1000").unwrap();
                }
                2 => {
                    let entry = changed.remove("file-1000").unwrap();
                    changed.insert("file-1000-renamed".into(), entry);
                }
                _ => unreachable!(),
            }
            let after = write(&changed, &objects).unwrap();
            // The first three groups contain at most 768 entries. All precede
            // these edits, without assuming constant-size insertion churn.
            assert_eq!(&before.pages[..3], &after.pages[..3]);
            assert_eq!(write(&changed, &objects).unwrap(), after);
            assert_eq!(
                read(&after, &packed::Reader::open(&objects).unwrap()).unwrap(),
                changed
            );
        }

        // The emitter must match serde even for escaped strings, UTF-8 paths,
        // wide signed timestamps, symlink byte arrays and optional link groups.
        let mut escaped = original.clone();
        let mut entry = escaped["file-0000"].clone();
        entry.id = "escaped-\"-\\-\n".into();
        entry.link_group = Some("linked-\"-\\".into());
        escaped.insert("é-雪".into(), entry);
        let (raw, ends, _) = planned(&escaped);
        assert_eq!(parse_canonical(&raw, MAX_ENTRIES).unwrap(), (escaped, ends));

        let small: Manifest = original.into_iter().take(334).collect();
        let (raw, _, lengths) = planned(&small);
        assert!(lengths.len() > 1 && raw.len() < MAX_PAGE_BYTES);
        for wrong_lengths in [vec![raw.len()], vec![1, raw.len() - 1]] {
            let root = split_root(&raw, &wrong_lengths, &objects);
            validate_root(&root).unwrap();
            let error = read(&root, &packed::Reader::open(&objects).unwrap()).unwrap_err();
            assert!(error.to_string().contains("page boundaries"), "{error}");
        }
    }

    #[test]
    fn oversized_entry_fragments_reset_without_moving_later_groups() {
        let temp = temp_dir();
        let objects = temp.0.join("objects");
        let mut manifest = fixture();
        let small = write(&manifest, &objects).unwrap();
        let (_, small_ends, _) = planned(&manifest);
        // Each serialized chunk hash occupies 67 bytes including its separator.
        // This makes an actual entry just over two page caps, not a 64 MiB test.
        let count = 2 * MAX_PAGE_BYTES / 67 + 1;
        manifest.get_mut("file-0300").unwrap().kind = EntryKind::File {
            size: count as u64,
            chunks: vec!["a".repeat(64); count],
        };
        let (_, ends, _) = planned(&manifest);
        let index = manifest
            .keys()
            .position(|path| path == "file-0300")
            .unwrap();
        let entry_bytes = ends[index] - ends[index - 1];
        assert!(entry_bytes > 2 * MAX_PAGE_BYTES);
        assert!(entry_bytes < 2 * MAX_PAGE_BYTES + MIN_OBJECT_CHUNK_BYTES);
        let before = write(&manifest, &objects).unwrap();
        assert_eq!(
            read(&before, &packed::Reader::open(&objects).unwrap()).unwrap(),
            manifest
        );
        let mut group_entries = 0;
        let group_end = manifest
            .keys()
            .enumerate()
            .find_map(|(position, path)| {
                group_entries += 1;
                if group_entries == MAX_GROUP_ENTRIES
                    || (group_entries >= MIN_GROUP_ENTRIES && path_anchor(path))
                {
                    group_entries = 0;
                    if position >= index {
                        return Some(position);
                    }
                }
                None
            })
            .unwrap();
        let suffix = |root: &StoredManifest, end: usize| {
            let mut total = 0;
            root.pages
                .iter()
                .position(|page| {
                    total += page.raw_bytes as usize;
                    total == end
                })
                .unwrap()
                + 1
        };
        let small_suffix = suffix(&small, small_ends[group_end]);
        let large_suffix = suffix(&before, ends[group_end]);
        assert!(small_suffix < small.pages.len());
        assert_eq!(&small.pages[small_suffix..], &before.pages[large_suffix..]);
        let mut offset = 0;
        let tail_page = before
            .pages
            .iter()
            .position(|page| {
                offset += page.raw_bytes as usize;
                offset == ends[index]
            })
            .unwrap();
        assert_eq!(
            before.pages[tail_page - 1].raw_bytes as usize,
            MAX_PAGE_BYTES
        );
        assert_eq!(
            before.pages[tail_page - 2].raw_bytes as usize,
            MAX_PAGE_BYTES
        );
        assert!((before.pages[tail_page].raw_bytes as usize) < MIN_OBJECT_CHUNK_BYTES);
        assert!(tail_page + 1 < before.pages.len());
        let EntryKind::File { size, chunks } = &mut manifest.get_mut("file-0300").unwrap().kind
        else {
            unreachable!();
        };
        *size += 1;
        chunks.push("b".repeat(64));
        let after = write(&manifest, &objects).unwrap();
        assert_eq!(
            &before.pages[tail_page + 1..],
            &after.pages[tail_page + 1..]
        );
        assert_ne!(before.pages[tail_page].hash, after.pages[tail_page].hash);
        let mut point = packed::PointReader::open(&objects).unwrap();
        assert_eq!(read_point(&after, &mut point).unwrap(), manifest);
    }

    #[test]
    fn encoded_envelope_rejects_before_object_reads() {
        // Exercise incompressible data around the locked encoder's block cuts.
        // These representative checks complement, not replace, the bound proof.
        let mut random = 0x9e3779b97f4a7c15_u64;
        for length in [1, 8191, 8192, 131071, 131072, MAX_PAGE_BYTES] {
            let bytes: Vec<u8> = (0..length)
                .map(|_| {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    random as u8
                })
                .collect();
            let encoded = metadata::encode(&bytes).unwrap();
            assert!(encoded.len() <= length + MAX_PAGE_ENCODING_OVERHEAD);
        }
        let temp = temp_dir();
        let objects = temp.0.join("objects");
        let original = fixture();
        let root = write(&original, &objects).unwrap();
        for size in [u64::MAX, root.pages[0].raw_bytes as u64 + 4097] {
            let mut bad = root.clone();
            bad.pages[0].encoded = EntryKind::File {
                size,
                chunks: vec!["0".repeat(64)],
            };
            assert!(validate_root(&bad).is_err());
            // No reader callback may run for an invalid declared envelope.
            let error = read_with(&bad, &mut |_| panic!("invalid root reached object IO"));
            assert!(error.is_err());
        }
        let mut aggregate = root.clone();
        let mut page = root.pages[0].clone();
        page.raw_bytes = 8192;
        page.encoded = EntryKind::File {
            size: 8192 + 4096,
            chunks: vec!["0".repeat(64)],
        };
        aggregate.pages = vec![page; MAX_PAGES];
        aggregate.raw_bytes = MAX_MANIFEST_BYTES as u64;
        let error =
            read_with(&aggregate, &mut |_| panic!("budget failure reached IO")).unwrap_err();
        assert!(error.to_string().contains("encoded byte budget"), "{error}");
        assert_eq!(MAX_TOTAL_ENCODED_BYTES, 128 * 1024 * 1024 + 36 * 1024);
    }

    #[test]
    fn path_page_caps_and_bounded_canonical_sink() {
        assert_eq!(MAX_PAGES, 16_393);
        let source = fixture();
        let single: Manifest = source
            .iter()
            .take(1)
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        // Synthetic spans exercise the planner at the exact 64 MiB cap without
        // allocating a second huge JSON document or claiming a huge roundtrip.
        for bytes in [
            MAX_PAGE_BYTES - 1,
            MAX_PAGE_BYTES,
            MAX_PAGE_BYTES + 1,
            2 * MAX_PAGE_BYTES + 1,
            MAX_MANIFEST_BYTES,
        ] {
            let lengths = page_lengths(&single, &[bytes]).unwrap();
            assert_eq!(lengths.iter().sum::<usize>(), bytes);
            assert_eq!(lengths.len(), bytes.div_ceil(MAX_PAGE_BYTES));
            assert!(
                lengths
                    .iter()
                    .all(|&length| length != 0 && length <= MAX_PAGE_BYTES)
            );
        }
        assert!(page_lengths(&single, &[MAX_MANIFEST_BYTES + 1]).is_err());
        assert!(page_lengths(&single, &[0]).is_err());
        assert!(page_lengths(&single, &[]).is_err());
        let pair: Manifest = source
            .iter()
            .take(2)
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        let half = MAX_PAGE_BYTES / 2 + 1;
        assert_eq!(
            page_lengths(&pair, &[half, 2 * half]).unwrap(),
            vec![half, half]
        );

        let no_anchors: Manifest = source
            .iter()
            .filter(|(path, _)| path.is_empty() || !path_anchor(path))
            .take(MAX_GROUP_ENTRIES + 1)
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        assert_eq!(no_anchors.len(), MAX_GROUP_ENTRIES + 1);
        let tiny_ends: Vec<usize> = (1..=no_anchors.len()).collect();
        assert_eq!(page_lengths(&no_anchors, &tiny_ends).unwrap(), vec![256, 1]);
        assert!(page_lengths(&no_anchors, &vec![1; no_anchors.len()]).is_err());

        // Alternating tiny and oversized spans stress the page-bound proof.
        let alternating: Manifest = source
            .iter()
            .take(511)
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect();
        let mut end = 0;
        let ends: Vec<usize> = (0..alternating.len())
            .map(|index| {
                end += if index % 2 == 0 {
                    1
                } else {
                    MAX_PAGE_BYTES + 1
                };
                end
            })
            .collect();
        let lengths = page_lengths(&alternating, &ends).unwrap();
        assert!(end <= MAX_MANIFEST_BYTES);
        assert_eq!(lengths.iter().sum::<usize>(), end);
        assert!(
            lengths.len()
                <= alternating.len().div_ceil(MIN_GROUP_ENTRIES) + 3 * end.div_ceil(MAX_PAGE_BYTES)
        );

        let canonical = serde_json::to_vec(&single).unwrap();
        for limit in [canonical.len() - 1, canonical.len()] {
            let mut output = LimitedBytes {
                bytes: Vec::new(),
                limit,
            };
            let result = emit_canonical(&single, &mut output);
            assert_eq!(result.is_ok(), limit == canonical.len());
            assert!(output.bytes.len() <= limit);
        }
        assert!(parse_canonical(&canonical, 0).is_err());
        assert_eq!(parse_canonical(&canonical, 1).unwrap().0, single);
    }
}
