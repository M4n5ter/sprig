//! Whole-store accounting never traverses a virtual mounted workspace tree.

use super::*;
use crate::StorageUsage;

impl Store {
    pub(super) fn storage_usage(&self) -> Result<BTreeMap<String, StorageUsage>> {
        let mut result = BTreeMap::new();
        let root = platform::symlink_metadata(&self.root)?;
        result.insert(
            ".".into(),
            StorageUsage {
                files: 0,
                logical_bytes: root.len(),
                allocated_bytes: Some(platform::allocated_bytes(&self.root, &root)?),
            },
        );
        for child in fs::read_dir(&self.root)? {
            let child = child?;
            let name = child
                .file_name()
                .into_string()
                .map_err(|_| failure("invalid store filename"))?;
            let mut usage = StorageUsage {
                files: 0,
                logical_bytes: 0,
                allocated_bytes: Some(0),
            };
            let mut seen = BTreeSet::new();
            measure(
                &child.path(),
                &mut usage,
                &mut seen,
                name == "native",
                false,
            )?;
            result.insert(name, usage);
        }
        Ok(result)
    }
}

fn measure(
    path: &Path,
    usage: &mut StorageUsage,
    seen: &mut BTreeSet<(u64, u64)>,
    generations: bool,
    generation: bool,
) -> Result<()> {
    let metadata = platform::symlink_metadata(path)?;
    if metadata.is_file() && !seen.insert((metadata.dev(), metadata.ino())) {
        return Ok(());
    }
    usage.logical_bytes = usage
        .logical_bytes
        .checked_add(metadata.len())
        .ok_or_else(|| failure("storage accounting overflow"))?;
    if let Some(bytes) = &mut usage.allocated_bytes {
        *bytes = bytes
            .checked_add(platform::allocated_bytes(path, &metadata)?)
            .ok_or_else(|| failure("storage allocation overflow"))?;
    }
    if metadata.is_file() {
        usage.files += 1;
    }
    if metadata.is_dir() {
        // Lower/mapped trees are views of already counted CAS/journal/upper bytes.
        // Skip even when unmounted so measurement does not depend on mount state.
        let virtual_tree = generation
            && (path.join("projection-base").try_exists()?
                || path.join("writable-base").try_exists()?);
        for child in fs::read_dir(path)? {
            let child = child?;
            if virtual_tree && (child.file_name() == "tree" || child.file_name() == "lower") {
                continue;
            }
            measure(&child.path(), usage, seen, false, generations)?;
        }
    }
    Ok(())
}
