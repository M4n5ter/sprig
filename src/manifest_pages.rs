//! Shared immutable pages for the canonical, sorted flat manifest JSON stream.
//!
//! This is a bounded representation, not a second manifest model. CDC cuts may
//! split JSON records. Reads reassemble and validate the exact canonical bytes
//! before returning the ordinary public `Manifest`. A writer finishes its one
//! object batch before returning any usable root; the caller still owns durable
//! root/revision publication. Readers reuse one caller-owned packed snapshot.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, BufRead, Cursor, Read, Write};
use std::path::Path;

use fastcdc::v2020::FastCDC;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::content::{self, Entry, EntryKind, Manifest, ObjectBatch};
use crate::{Result, metadata, packed};

const MIN_PAGE_BYTES: usize = 16 * 1024;
const AVG_PAGE_BYTES: usize = 64 * 1024;
const MAX_PAGE_BYTES: usize = 256 * 1024;
// Bound decoded metadata and references. Never silently truncate a larger manifest.
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_PAGES: usize = MAX_MANIFEST_BYTES / MIN_PAGE_BYTES;
const MAX_ENCODED_PAGE_BYTES: usize = 512 * 1024;
const MAX_PAGE_CHUNKS: usize = MAX_ENCODED_PAGE_BYTES / MIN_PAGE_BYTES + 1;
const MANIFEST_DOMAIN: &[u8] = b"sprig-manifest-pages-v1\0";
const PAGE_DOMAIN: &[u8] = b"sprig-manifest-json-page-v1\0";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ManifestFormat {
    #[serde(rename = "sprig_manifest_pages_v1")]
    FlatJsonCdcZstdV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum PageFormat {
    #[serde(rename = "sprig_manifest_json_page_v1")]
    JsonFragmentZstdV1,
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

fn page_length(bytes: &[u8]) -> usize {
    let input = &bytes[..bytes.len().min(MAX_PAGE_BYTES)];
    FastCDC::new(
        input,
        MIN_PAGE_BYTES as _,
        AVG_PAGE_BYTES as _,
        MAX_PAGE_BYTES as _,
    )
    .next()
    .expect("nonempty manifest JSON has a CDC page")
    .length
}

/// Store compressed CDC pages and finish the single durability batch before
/// returning their root. The caller must then durably publish that root.
pub(crate) fn write(manifest: &Manifest, objects: &Path) -> Result<StoredManifest> {
    if manifest.len() > MAX_ENTRIES {
        return Err(invalid("manifest exceeds the entry-count limit"));
    }
    content::validate_manifest(manifest)?;
    let mut output = LimitedBytes {
        bytes: Vec::new(),
        limit: MAX_MANIFEST_BYTES,
    };
    serde_json::to_writer(&mut output, manifest)?;
    let raw = output.bytes;
    let mut root = StoredManifest {
        format: ManifestFormat::FlatJsonCdcZstdV1,
        raw_bytes: raw.len() as u64,
        hash: digest(MANIFEST_DOMAIN, &raw),
        pages: Vec::new(),
    };
    let mut batch = ObjectBatch::open(objects)?;
    let mut remaining = raw.as_slice();
    while !remaining.is_empty() {
        let length = page_length(remaining);
        let bytes = &remaining[..length];
        let encoded = metadata::encode(bytes)?;
        if encoded.len() > MAX_ENCODED_PAGE_BYTES || root.pages.len() == MAX_PAGES {
            return Err(invalid("manifest exceeds the page representation limit"));
        }
        root.pages.push(StoredPage {
            format: PageFormat::JsonFragmentZstdV1,
            raw_bytes: length as u32,
            hash: digest(PAGE_DOMAIN, bytes),
            encoded: batch.put_reader(&mut Cursor::new(encoded))?,
        });
        remaining = &remaining[length..];
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
    for (number, page) in root.pages.iter().enumerate() {
        let length = page.raw_bytes as usize;
        if length == 0
            || length > MAX_PAGE_BYTES
            || (number + 1 != root.pages.len() && length < MIN_PAGE_BYTES)
            || !valid_hash(&page.hash)
        {
            return Err(invalid("invalid manifest page bounds or hash"));
        }
        let EntryKind::File { size, chunks } = &page.encoded else {
            return Err(invalid("manifest page must have file storage"));
        };
        if *size == 0
            || *size > MAX_ENCODED_PAGE_BYTES as u64
            || chunks.is_empty()
            || chunks.len() > MAX_PAGE_CHUNKS
            || chunks.len() as u64 > *size
            || *size > (chunks.len() as u64) * MAX_PAGE_BYTES as u64
            || !chunks.iter().all(|hash| valid_hash(hash))
        {
            return Err(invalid("invalid encoded manifest page storage"));
        }
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

fn parse_canonical(raw: &[u8], entry_limit: usize) -> Result<Manifest> {
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let manifest =
        Deserializer::deserialize_map(&mut decoder, OrderedManifestVisitor { entry_limit })?;
    decoder.end()?;
    content::validate_manifest(&manifest)?;
    let mut check = CanonicalCheck {
        expected: raw,
        offset: 0,
    };
    serde_json::to_writer(&mut check, &manifest)?;
    if check.offset != raw.len() {
        return Err(invalid("trailing noncanonical manifest JSON"));
    }
    Ok(manifest)
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
    let mut remaining = raw.as_slice();
    for page in &root.pages {
        if page_length(remaining) != page.raw_bytes as usize {
            return Err(invalid("noncanonical manifest CDC page boundaries"));
        }
        remaining = &remaining[page.raw_bytes as usize..];
    }
    parse_canonical(&raw, MAX_ENTRIES)
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
    fn fixture() -> Manifest {
        let mut manifest = Manifest::new();
        manifest.insert(
            "".into(),
            Entry {
                id: "root".into(),
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
            format: ManifestFormat::FlatJsonCdcZstdV1,
            raw_bytes: raw.len() as u64,
            hash: digest(MANIFEST_DOMAIN, raw),
            pages: vec![StoredPage {
                format: PageFormat::JsonFragmentZstdV1,
                raw_bytes: raw.len() as u32,
                hash: digest(PAGE_DOMAIN, raw),
                encoded: storage,
            }],
        }
    }

    #[test]
    fn roundtrip_corruption_and_limits() {
        let path = std::env::temp_dir().join(format!(
            "sprig-manifest-pages-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let temp = TestDir(path);
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
        let wrong_domain = String::from_utf8(encoded_root.clone())
            .unwrap()
            .replace("sprig_manifest_pages_v1", "sprig_manifest_pages_v2");
        assert!(serde_json::from_str::<StoredManifest>(&wrong_domain).is_err());
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
}
