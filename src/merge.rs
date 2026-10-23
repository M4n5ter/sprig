//! Identity-aware three-way merging with explicit timestamp-based LWW policies.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};

use crate::content::{self, Entry, EntryKind, Manifest};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeStrategy {
    Conflict,
    Lww,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeConflict {
    pub path: String,
    pub kind: String,
    pub base: Option<Entry>,
    pub ours: Option<Entry>,
    pub theirs: Option<Entry>,
}

#[derive(Clone, Debug)]
pub struct MergeResult {
    pub manifest: Manifest,
    pub conflicts: Vec<MergeConflict>,
}

// A binding is relative to a stable parent identity, not its current pathname.
// Thus editing or adding a child follows a concurrent parent-directory move.
#[derive(Clone, Eq, PartialEq)]
struct Value {
    parent: Option<String>,
    name: String,
    entry: Entry,
}

#[derive(Clone, Eq, PartialEq)]
struct Version {
    path: String,
    value: Value,
}

struct Node {
    versions: [Option<Version>; 3],
    value: Option<Value>,
    issues: BTreeSet<&'static str>,
    automatic: bool,
    choice: Option<usize>,
}

struct Groups(Vec<usize>);

impl Groups {
    fn root(&self, mut index: usize) -> usize {
        while self.0[index] != index {
            index = self.0[index];
        }
        index
    }

    fn join(&mut self, first: usize, second: usize) -> bool {
        let first = self.root(first);
        let second = self.root(second);
        if first == second {
            return false;
        }
        self.0[first.max(second)] = first.min(second);
        true
    }
}

/// Merge immutable manifests without changing either input.
///
/// Global LWW resolves conflicts using the newer modification time, tying to
/// ours. A `.lww` file in ours selects this policy for matching bilateral changes.
/// Atomic units skip content merging and select a complete side by newest member
/// time; the first matching unit owns overlapping entries. Deleted entries use
/// their nearest surviving parent directory's time. Structural conflicts are
/// resolved as a whole component, never as a corrupt mixture of paths.
/// Unresolved components return an ours-side fallback and conflict records;
/// callers must refuse publication whenever those records are nonempty.
#[allow(clippy::too_many_arguments)]
pub fn merge(
    base: &Manifest,
    ours: &Manifest,
    theirs: &Manifest,
    objects: &Path,
    strategy: MergeStrategy,
    _ours_order: (u64, &str),
    _theirs_order: (u64, &str),
    units: &[Vec<String>],
) -> Result<MergeResult> {
    for manifest in [base, ours, theirs] {
        content::validate_manifest(manifest)?;
    }
    validate_units(units)?;
    let mut content = MergeContent {
        reader: crate::packed::Reader::open(objects)?,
        objects,
        batch: None,
    };
    let rules = read_rules(ours, &content.reader)?;
    let (ours, theirs) = align_additions(base, ours, theirs);
    let versions = [index(base), index(&ours), index(&theirs)];
    let ids: BTreeSet<_> = versions.iter().flat_map(|v| v.keys().cloned()).collect();
    let indexes: BTreeMap<_, _> = ids.into_iter().enumerate().map(|(i, id)| (id, i)).collect();
    let mut nodes = Vec::with_capacity(indexes.len());
    for id in indexes.keys() {
        let node = Node {
            versions: std::array::from_fn(|i| versions[i].get(id).cloned()),
            value: None,
            issues: BTreeSet::new(),
            automatic: false,
            choice: None,
        };
        nodes.push(node);
    }
    let mut groups = Groups((0..nodes.len()).collect());
    let timestamps: Vec<[i128; 2]> = (0..nodes.len())
        .map(|i| {
            [
                side_time(i, 1, &nodes, &indexes),
                side_time(i, 2, &nodes, &indexes),
            ]
        })
        .collect();
    // Namespace identities remain distinct; inode payloads are merged once for
    // every connected hardlink component, including aliases removed on a side.
    let link_components = hardlink_components(&nodes, &mut groups);
    let linked: BTreeSet<_> = link_components.iter().flatten().copied().collect();
    for node in &mut nodes {
        let version = node.versions[1]
            .as_ref()
            .or(node.versions[2].as_ref())
            .or(node.versions[0].as_ref())
            .ok_or("empty identity")?;
        node.automatic = rule_matches(
            &rules,
            &version.path,
            matches!(version.value.entry.kind, EntryKind::Directory),
        );
    }
    add_units(&mut nodes, &mut groups, units, &timestamps);
    for component in &link_components {
        merge_hardlinks(&mut nodes, component, &mut content, &timestamps)?;
    }
    for (i, node) in nodes.iter_mut().enumerate() {
        if node.choice.is_none() && !linked.contains(&i) {
            let same = |a: usize, b: usize| {
                node.versions[a].as_ref().map(|v| &v.value)
                    == node.versions[b].as_ref().map(|v| &v.value)
            };
            if node.automatic && !same(0, 1) && !same(0, 2) && !same(1, 2) {
                node.choice = Some(newer_side(&[i], &timestamps));
            } else {
                merge_node(node, &mut content)?;
            }
        }
        if let Some(side) = node.choice {
            node.value = node.versions[side].as_ref().map(|v| v.value.clone());
        }
    }
    let unresolved = loop {
        let unresolved = resolve_groups(&mut nodes, &groups, strategy, &timestamps);
        let violations = structural_violations(&nodes, &indexes)?;
        if violations.is_empty() {
            break unresolved;
        }
        let children = all_children(&nodes, &indexes);
        let mut progressed = false;
        for (seeds, kind) in violations {
            let affected = descendants(&seeds, &children);
            let first = *affected.iter().next().ok_or("empty structural conflict")?;
            progressed |= nodes[first].issues.insert(kind);
            for index in affected {
                progressed |= groups.join(first, index);
            }
        }
        if !progressed {
            return Err("could not resolve a structural conflict to a valid input side".into());
        }
    };
    let manifest = materialize_paths(&nodes, &indexes)?;
    content::validate_manifest(&manifest)?;
    let paths_by_id: BTreeMap<&str, &str> = manifest
        .iter()
        .map(|(path, entry)| (entry.id.as_str(), path.as_str()))
        .collect();
    let mut conflicts = Vec::new();
    if strategy == MergeStrategy::Conflict {
        for (i, node) in nodes.iter().enumerate() {
            if !unresolved.contains(&groups.root(i)) {
                continue;
            }
            for kind in &node.issues {
                let first = node
                    .versions
                    .iter()
                    .flatten()
                    .next()
                    .ok_or("empty identity")?;
                conflicts.push(MergeConflict {
                    path: paths_by_id
                        .get(first.value.entry.id.as_str())
                        .copied()
                        .unwrap_or(&first.path)
                        .to_owned(),
                    kind: (*kind).to_owned(),
                    base: node.versions[0].as_ref().map(|v| v.value.entry.clone()),
                    ours: node.versions[1].as_ref().map(|v| v.value.entry.clone()),
                    theirs: node.versions[2].as_ref().map(|v| v.value.entry.clone()),
                });
            }
        }
        conflicts.sort_by(|a, b| (&a.path, &a.kind).cmp(&(&b.path, &b.kind)));
    }
    if let Some(batch) = content.batch {
        batch.finish()?;
    }
    Ok(MergeResult {
        manifest,
        conflicts,
    })
}

// One immutable catalog and at most one durable write batch per tree merge.
struct MergeContent<'a> {
    objects: &'a Path,
    reader: crate::packed::Reader,
    batch: Option<content::ObjectBatch>,
}

impl MergeContent<'_> {
    fn read(&self, entry: &Entry) -> Result<Vec<u8>> {
        content::read_file_from(entry, &self.reader)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<EntryKind> {
        if self.batch.is_none() {
            self.batch = Some(content::ObjectBatch::open(self.objects)?);
        }
        self.batch
            .as_mut()
            .expect("batch opened")
            .put_reader(&mut std::io::Cursor::new(bytes))
    }
}

fn index(manifest: &Manifest) -> BTreeMap<String, Version> {
    manifest
        .iter()
        .map(|(path, entry)| {
            let (parent, name) = if path.is_empty() {
                (None, String::new())
            } else {
                let (parent_path, name) = path.rsplit_once('/').unwrap_or(("", path));
                (Some(manifest[parent_path].id.clone()), name.to_owned())
            };
            (
                entry.id.clone(),
                Version {
                    path: path.clone(),
                    value: Value {
                        parent,
                        name,
                        entry: entry.clone(),
                    },
                },
            )
        })
        .collect()
}

// Independently added directories at the same path can have different IDs.
// Unify those IDs (and identical file additions) before building parent bindings.
fn align_additions(base: &Manifest, ours: &Manifest, theirs: &Manifest) -> (Manifest, Manifest) {
    let base_ids: BTreeSet<_> = base.values().map(|e| &e.id).collect();
    let ours_ids: BTreeSet<_> = ours.values().map(|e| &e.id).collect();
    let theirs_ids: BTreeSet<_> = theirs.values().map(|e| &e.id).collect();
    let mut aliases = BTreeMap::new();
    for (path, left) in ours {
        let Some(right) = theirs.get(path) else {
            continue;
        };
        if left.id == right.id
            || base_ids.contains(&left.id)
            || base_ids.contains(&right.id)
            || theirs_ids.contains(&left.id)
            || ours_ids.contains(&right.id)
        {
            continue;
        }
        let directories = matches!(
            (&left.kind, &right.kind),
            (EntryKind::Directory, EntryKind::Directory)
        );
        if directories
            || (left.mode == right.mode
                && left.kind == right.kind
                && left.link_group == right.link_group)
        {
            let canonical = left.id.clone().min(right.id.clone());
            aliases.insert(left.id.clone(), canonical.clone());
            aliases.insert(right.id.clone(), canonical);
        }
    }
    let apply = |manifest: &Manifest| {
        manifest
            .iter()
            .map(|(path, entry)| {
                let mut entry = entry.clone();
                if let Some(canonical) = aliases.get(&entry.id) {
                    entry.id = canonical.clone();
                }
                (path.clone(), entry)
            })
            .collect()
    };
    (apply(ours), apply(theirs))
}

// A singleton's entry ID is also its initial inode identity. On first link,
// capture/writable persist that identity as link_group; later path replacement
// must assign a fresh group when retaining the entry ID would alias another inode.
fn inode_key(entry: &Entry) -> Option<&str> {
    matches!(entry.kind, EntryKind::File { .. })
        .then(|| entry.link_group.as_deref().unwrap_or(&entry.id))
}

fn hardlink_components(nodes: &[Node], groups: &mut Groups) -> Vec<Vec<usize>> {
    let mut owners = BTreeMap::new();
    let mut linked = BTreeSet::new();
    for (i, node) in nodes.iter().enumerate() {
        for version in node.versions.iter().flatten() {
            if let Some(key) = inode_key(&version.value.entry) {
                if let Some(&previous) = owners.get(key) {
                    groups.join(previous, i);
                } else {
                    owners.insert(key.to_owned(), i);
                }
                if version.value.entry.link_group.is_some() {
                    linked.insert(i);
                }
            }
        }
    }
    let roots: BTreeSet<_> = linked.into_iter().map(|i| groups.root(i)).collect();
    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..nodes.len() {
        let root = groups.root(i);
        if roots.contains(&root) {
            components.entry(root).or_default().push(i);
        }
    }
    components.into_values().collect()
}

fn payload_version(entry: &Entry, key: &str) -> Version {
    let mut entry = entry.clone();
    entry.id = key.to_owned();
    entry.link_group = None;
    Version {
        path: String::new(),
        value: Value {
            parent: None,
            name: String::new(),
            entry,
        },
    }
}

fn merge_hardlinks(
    nodes: &mut [Node],
    members: &[usize],
    objects: &mut MergeContent<'_>,
    timestamps: &[[i128; 2]],
) -> Result<()> {
    if members.iter().any(|&i| nodes[i].choice.is_some()) {
        // add_units expands every selection to complete hardlink components.
        return Ok(());
    }
    let same = |a: usize, b: usize| {
        members
            .iter()
            .all(|&i| nodes[i].versions[a] == nodes[i].versions[b])
    };
    if members.iter().all(|&i| nodes[i].automatic) && !same(0, 1) && !same(0, 2) && !same(1, 2) {
        let winner = newer_side(members, timestamps);
        for &i in members {
            nodes[i].choice = Some(winner);
        }
        return Ok(());
    }

    let mut payloads: BTreeMap<String, [Option<Entry>; 3]> = BTreeMap::new();
    let mut persistent = BTreeSet::new();
    for &i in members {
        for (side, version) in nodes[i].versions.iter().enumerate() {
            let Some(version) = version else { continue };
            let entry = &version.value.entry;
            let Some(key) = inode_key(entry) else {
                continue;
            };
            let versions = payloads.entry(key.to_owned()).or_default();
            // Input validation guarantees every alias in one version agrees.
            versions[side].get_or_insert_with(|| entry.clone());
            if entry.link_group.is_some() {
                persistent.insert(key.to_owned());
            }
        }
    }
    // First merge only names, parent bindings and inode references. An unlink
    // removes a directory entry, not the shared payload still used by an alias.
    for &i in members {
        let mut namespace = Node {
            versions: nodes[i].versions.clone(),
            value: None,
            issues: BTreeSet::new(),
            automatic: false,
            choice: None,
        };
        for version in namespace.versions.iter_mut().flatten() {
            let entry = &mut version.value.entry;
            if let Some(key) = inode_key(entry).map(str::to_owned) {
                entry.link_group = Some(key);
                entry.kind = EntryKind::File {
                    size: 0,
                    chunks: Vec::new(),
                };
                entry.mode = 0;
                entry.modified_ns = 0;
            }
        }
        merge_node(&mut namespace, objects)?;
        nodes[i].value = namespace.value;
        nodes[i].issues.extend(namespace.issues);
    }
    let surviving: BTreeSet<String> = members
        .iter()
        .filter_map(|&i| nodes[i].value.as_ref())
        .filter_map(|value| inode_key(&value.entry).map(str::to_owned))
        .collect();
    let mut merged_payloads = BTreeMap::new();
    for (key, versions) in payloads {
        let mut payload = Node {
            versions: std::array::from_fn(|side| {
                versions[side]
                    .as_ref()
                    .map(|entry| payload_version(entry, &key))
            }),
            value: None,
            issues: BTreeSet::new(),
            automatic: false,
            choice: None,
        };
        // If a newly added or surviving alias retains this inode, deleting all
        // names on the other side does not discard the inode's concurrent edit.
        // Without a surviving name, retain ordinary delete/modify conflicts.
        if surviving.contains(&key)
            && versions[0].is_some()
            && (versions[1].is_none() || versions[2].is_none())
        {
            payload.value = payload.versions[1]
                .as_ref()
                .or(payload.versions[2].as_ref())
                .map(|v| v.value.clone());
        } else {
            merge_node(&mut payload, objects)?;
        }
        // A side can delete the final original alias while the other adds a
        // new alias without changing bytes. The namespace merge retains the
        // addition; the unchanged base payload supplies its contents.
        let entry = payload
            .value
            .map(|value| value.entry)
            .or_else(|| versions.iter().flatten().next().cloned())
            .ok_or("missing hardlink payload")?;
        merged_payloads.insert(key, entry);
        nodes[members[0]].issues.extend(payload.issues);
    }
    for &i in members {
        if let Some(value) = &mut nodes[i].value
            && let Some(key) = inode_key(&value.entry).map(str::to_owned)
        {
            let payload = merged_payloads
                .get(&key)
                .ok_or("missing merged inode payload")?;
            value.entry.kind = payload.kind.clone();
            value.entry.mode = payload.mode;
            value.entry.modified_ns = payload.modified_ns;
            value.entry.link_group = persistent.contains(&key).then_some(key);
        }
    }
    Ok(())
}

fn merge_node(node: &mut Node, objects: &mut MergeContent<'_>) -> Result<()> {
    let values: [Option<Value>; 3] =
        std::array::from_fn(|i| node.versions[i].as_ref().map(|v| v.value.clone()));
    let [base, ours, theirs] = &values;
    if ours == theirs || theirs == base {
        node.value = ours.clone();
        return Ok(());
    }
    if ours == base {
        node.value = theirs.clone();
        return Ok(());
    }
    let (Some(ours), Some(theirs)) = (ours, theirs) else {
        node.issues.insert("delete_modify");
        return Ok(());
    };
    let mut result = ours.clone();
    let our_binding = (&ours.parent, &ours.name);
    let their_binding = (&theirs.parent, &theirs.name);
    let base_binding = base.as_ref().map(|v| (&v.parent, &v.name));
    if let Some((parent, name)) = merge_field(base_binding.as_ref(), &our_binding, &their_binding) {
        result.parent = parent.clone();
        result.name = name.clone();
    } else {
        node.issues.insert("rename");
    }
    if let Some(group) = merge_field(
        base.as_ref().map(|v| &v.entry.link_group),
        &ours.entry.link_group,
        &theirs.entry.link_group,
    ) {
        result.entry.link_group = group;
    } else {
        node.issues.insert("hardlink_topology");
    }
    if let Some(mode) = merge_field(
        base.as_ref().map(|v| &v.entry.mode),
        &ours.entry.mode,
        &theirs.entry.mode,
    ) {
        result.entry.mode = mode;
    } else {
        node.issues.insert("mode");
    }
    result.entry.modified_ns = merge_field(
        base.as_ref().map(|v| &v.entry.modified_ns),
        &ours.entry.modified_ns,
        &theirs.entry.modified_ns,
    )
    .unwrap_or_else(|| ours.entry.modified_ns.max(theirs.entry.modified_ns));
    match merge_kind(
        base.as_ref().map(|v| &v.entry),
        &ours.entry,
        &theirs.entry,
        objects,
    )? {
        Ok(kind) => result.entry.kind = kind,
        Err(kind) => {
            node.issues.insert(kind);
        }
    }
    node.value = Some(result);
    Ok(())
}

fn merge_field<T: Clone + PartialEq>(base: Option<&T>, ours: &T, theirs: &T) -> Option<T> {
    if ours == theirs || base == Some(theirs) {
        Some(ours.clone())
    } else if base == Some(ours) {
        Some(theirs.clone())
    } else {
        None
    }
}

fn merge_kind(
    base: Option<&Entry>,
    ours: &Entry,
    theirs: &Entry,
    objects: &mut MergeContent<'_>,
) -> Result<std::result::Result<EntryKind, &'static str>> {
    if let Some(kind) = merge_field(base.map(|e| &e.kind), &ours.kind, &theirs.kind) {
        return Ok(Ok(kind));
    }
    let (
        EntryKind::File { size: our_size, .. },
        EntryKind::File {
            size: their_size, ..
        },
    ) = (&ours.kind, &theirs.kind)
    else {
        let kind = if matches!(
            (&ours.kind, &theirs.kind),
            (EntryKind::Symlink { .. }, EntryKind::Symlink { .. })
        ) {
            "symlink"
        } else {
            "type"
        };
        return Ok(Err(kind));
    };
    if *our_size > MAX_TEXT_BYTES || *their_size > MAX_TEXT_BYTES {
        return Ok(Err("content_size_limit"));
    }
    let our_bytes = objects.read(ours)?;
    let their_bytes = objects.read(theirs)?;
    if our_bytes == their_bytes {
        return Ok(Ok(ours.kind.clone()));
    }
    let Some(base) = base else {
        return Ok(Err("add_add"));
    };
    let EntryKind::File { size, .. } = &base.kind else {
        return Ok(Err("type"));
    };
    if *size > MAX_TEXT_BYTES {
        return Ok(Err("content_size_limit"));
    }
    let base_bytes = objects.read(base)?;
    if our_bytes == base_bytes {
        return Ok(Ok(theirs.kind.clone()));
    }
    if their_bytes == base_bytes {
        return Ok(Ok(ours.kind.clone()));
    }
    if [&base_bytes, &our_bytes, &their_bytes]
        .iter()
        .any(|bytes| bytes.contains(&0))
    {
        return Ok(Err("binary"));
    }
    let strings = (
        std::str::from_utf8(&base_bytes),
        std::str::from_utf8(&our_bytes),
        std::str::from_utf8(&their_bytes),
    );
    let (Ok(base_text), Ok(our_text), Ok(their_text)) = strings else {
        return Ok(Err("binary"));
    };
    match diffy::merge(base_text, our_text, their_text) {
        Ok(text) => Ok(Ok(objects.put(text.as_bytes())?)),
        Err(_) => Ok(Err("content")),
    }
}

fn validate_units(units: &[Vec<String>]) -> Result<()> {
    for path in units.iter().flatten() {
        if !path.is_empty()
            && (path.starts_with('/')
                || path.contains('\0')
                || path
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == ".."))
        {
            return Err(format!("invalid merge-unit path: {path:?}").into());
        }
    }
    Ok(())
}

fn within(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn add_units(
    nodes: &mut [Node],
    groups: &mut Groups,
    units: &[Vec<String>],
    timestamps: &[[i128; 2]],
) {
    for unit in units {
        let selected: BTreeSet<_> = nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                node.choice.is_none()
                    && node
                        .versions
                        .iter()
                        .flatten()
                        .any(|v| unit.iter().any(|path| within(&v.path, path)))
            })
            .map(|(i, _)| groups.root(i))
            .collect();
        // An atomic unit containing one alias owns its complete inode component.
        // Otherwise two units could choose conflicting bytes for the same inode.
        let members: Vec<_> = (0..nodes.len())
            .filter(|&i| nodes[i].choice.is_none() && selected.contains(&groups.root(i)))
            .collect();
        let Some(&first) = members.first() else {
            continue;
        };
        let same = |a: usize, b: usize| {
            members
                .iter()
                .all(|&i| nodes[i].versions[a] == nodes[i].versions[b])
        };
        let winner = if same(1, 2) || same(0, 2) {
            1
        } else if same(0, 1) {
            2
        } else {
            newer_side(&members, timestamps)
        };
        for index in members {
            groups.join(first, index);
            nodes[index].choice = Some(winner);
            nodes[index].automatic = true;
        }
    }
}

fn newer_side(members: &[usize], timestamps: &[[i128; 2]]) -> usize {
    let newest = |side| {
        members
            .iter()
            .map(|&i| timestamps[i][side])
            .max()
            .unwrap_or(i128::MIN)
    };
    if newest(0) >= newest(1) { 1 } else { 2 }
}

fn side_time(index: usize, side: usize, nodes: &[Node], indexes: &BTreeMap<String, usize>) -> i128 {
    if let Some(version) = &nodes[index].versions[side] {
        return version.value.entry.modified_ns;
    }
    let mut current = index;
    let mut visited = BTreeSet::new();
    while visited.insert(current) {
        if let Some(version) = &nodes[current].versions[side]
            && matches!(version.value.entry.kind, EntryKind::Directory)
        {
            return version.value.entry.modified_ns;
        }
        let reference = nodes[current].versions[0]
            .as_ref()
            .or(nodes[current].versions[3 - side].as_ref());
        let Some(parent) = reference
            .and_then(|v| v.value.parent.as_ref())
            .and_then(|p| indexes.get(p))
        else {
            break;
        };
        current = *parent;
    }
    nodes
        .iter()
        .filter_map(|n| n.versions[side].as_ref())
        .find(|v| v.value.parent.is_none())
        .map_or(i128::MIN, |v| v.value.entry.modified_ns)
}

fn resolve_groups(
    nodes: &mut [Node],
    groups: &Groups,
    strategy: MergeStrategy,
    timestamps: &[[i128; 2]],
) -> BTreeSet<usize> {
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..nodes.len() {
        members.entry(groups.root(i)).or_default().push(i);
    }
    let mut unresolved = BTreeSet::new();
    for (root, indexes) in members {
        if !indexes.iter().any(|&i| !nodes[i].issues.is_empty()) {
            continue;
        }
        let automatic =
            strategy == MergeStrategy::Lww || indexes.iter().all(|&i| nodes[i].automatic);
        let winner = if automatic {
            newer_side(&indexes, timestamps)
        } else {
            unresolved.insert(root);
            1
        };
        for i in indexes {
            nodes[i].value = nodes[i].versions[winner].as_ref().map(|v| v.value.clone());
        }
    }
    unresolved
}

// Match each rule independently so the last matching rule wins even when an
// earlier rule matched a child but a later rule matches a parent directory.
fn rule_matches(rules: &[Gitignore], path: &str, is_directory: bool) -> bool {
    for rule in rules.iter().rev() {
        let matched = rule.matched_path_or_any_parents(path, is_directory);
        if matched.is_ignore() {
            return true;
        }
        if matched.is_whitelist() {
            return false;
        }
    }
    false
}

fn read_rules(manifest: &Manifest, objects: &crate::packed::Reader) -> Result<Vec<Gitignore>> {
    if !manifest.contains_key(".lww") {
        return Ok(Vec::new());
    }
    let mut rules = Vec::new();
    read_rule_file(
        ".lww",
        manifest,
        objects,
        &mut BTreeSet::new(),
        &mut 0,
        &mut rules,
    )?;
    Ok(rules)
}

fn read_rule_file(
    path: &str,
    manifest: &Manifest,
    objects: &crate::packed::Reader,
    active: &mut BTreeSet<String>,
    bytes_read: &mut usize,
    rules: &mut Vec<Gitignore>,
) -> Result<()> {
    if active.len() >= 16 || !active.insert(path.to_owned()) {
        return Err(".lww include cycle or depth limit (16)".into());
    }
    let entry = manifest
        .get(path)
        .ok_or_else(|| format!("missing .lww include: {path}"))?;
    let EntryKind::File { size, .. } = &entry.kind else {
        return Err(format!(".lww rules must be regular files: {path}").into());
    };
    if *size > 1024 * 1024 || bytes_read.saturating_add(*size as usize) > 1024 * 1024 {
        return Err(".lww rules exceed 1 MiB aggregate limit".into());
    }
    let bytes = content::read_file_from(entry, objects)?;
    *bytes_read += bytes.len();
    for line in std::str::from_utf8(&bytes)?.lines() {
        let trimmed = line.trim();
        if trimmed.split_whitespace().next() == Some("%include") {
            let include = trimmed[8..].trim();
            if include.is_empty() {
                return Err("empty .lww include path".into());
            }
            validate_units(&[vec![include.to_owned()]])?;
            let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
            let include = if parent.is_empty() {
                include.to_owned()
            } else {
                format!("{parent}/{include}")
            };
            read_rule_file(&include, manifest, objects, active, bytes_read, rules)?;
        } else {
            if rules.len() >= 4096 {
                return Err(".lww rules exceed 4096-pattern limit".into());
            }
            let mut builder = GitignoreBuilder::new("");
            builder.add_line(Some(path.into()), line)?;
            let rule = builder.build()?;
            if !rule.is_empty() {
                rules.push(rule);
            }
        }
    }
    active.remove(path);
    Ok(())
}

fn structural_violations(
    nodes: &[Node],
    indexes: &BTreeMap<String, usize>,
) -> Result<Vec<(Vec<usize>, &'static str)>> {
    let mut problems = Vec::new();
    let mut bindings = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        let Some(value) = &node.value else { continue };
        if let Some(other) = bindings.insert((&value.parent, &value.name), i) {
            problems.push((vec![i, other], "path_collision"));
        }
        if let Some(parent) = &value.parent {
            let parent_index = *indexes.get(parent).ok_or("unknown parent identity")?;
            if !nodes[parent_index]
                .value
                .as_ref()
                .is_some_and(|v| matches!(v.entry.kind, EntryKind::Directory))
            {
                problems.push((vec![i, parent_index], "parent_delete_or_type"));
            }
        }
    }
    let mut states = vec![0; nodes.len()];
    for start in 0..nodes.len() {
        if states[start] != 0 || nodes[start].value.is_none() {
            continue;
        }
        let mut chain = Vec::new();
        let mut current = Some(start);
        while let Some(i) = current {
            if states[i] == 2 {
                break;
            }
            if states[i] == 1 {
                let offset = chain
                    .iter()
                    .position(|&item| item == i)
                    .ok_or("invalid parent traversal")?;
                problems.push((chain[offset..].to_vec(), "directory_cycle"));
                break;
            }
            states[i] = 1;
            chain.push(i);
            current = nodes[i]
                .value
                .as_ref()
                .and_then(|v| v.parent.as_ref())
                .and_then(|id| indexes.get(id).copied());
        }
        for i in chain {
            states[i] = 2;
        }
    }
    Ok(problems)
}

fn all_children(nodes: &[Node], indexes: &BTreeMap<String, usize>) -> Vec<BTreeSet<usize>> {
    let mut children = vec![BTreeSet::new(); nodes.len()];
    for (i, node) in nodes.iter().enumerate() {
        for value in node
            .versions
            .iter()
            .flatten()
            .map(|v| &v.value)
            .chain(node.value.iter())
        {
            if let Some(parent) = value.parent.as_ref().and_then(|id| indexes.get(id)) {
                children[*parent].insert(i);
            }
        }
    }
    children
}

fn descendants(seeds: &[usize], children: &[BTreeSet<usize>]) -> BTreeSet<usize> {
    let mut found = BTreeSet::new();
    let mut queue: VecDeque<_> = seeds.iter().copied().collect();
    while let Some(i) = queue.pop_front() {
        if found.insert(i) {
            queue.extend(children[i].iter().copied());
        }
    }
    found
}

fn materialize_paths(nodes: &[Node], indexes: &BTreeMap<String, usize>) -> Result<Manifest> {
    let mut paths: Vec<Option<String>> = vec![None; nodes.len()];
    let mut manifest = Manifest::new();
    for (i, node) in nodes.iter().enumerate() {
        let Some(value) = &node.value else { continue };
        let mut chain = Vec::new();
        let mut current = i;
        while paths[current].is_none() {
            chain.push(current);
            let item = nodes[current]
                .value
                .as_ref()
                .ok_or("missing merged parent")?;
            let Some(parent) = &item.parent else { break };
            current = *indexes.get(parent).ok_or("unknown merged parent")?;
        }
        for index in chain.into_iter().rev() {
            let item = nodes[index].value.as_ref().ok_or("missing merged entry")?;
            let path = match &item.parent {
                None => String::new(),
                Some(parent) => {
                    let prefix = paths[indexes[parent]]
                        .as_ref()
                        .ok_or("unresolved merged parent")?;
                    if prefix.is_empty() {
                        item.name.clone()
                    } else {
                        format!("{prefix}/{}", item.name)
                    }
                }
            };
            paths[index] = Some(path);
        }
        if manifest
            .insert(
                paths[i].clone().ok_or("missing merged path")?,
                value.entry.clone(),
            )
            .is_some()
        {
            return Err("duplicate merged path".into());
        }
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_text_units_and_structural_resolution() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "sprig-merge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&directory)?;
        // macOS temporary paths may start with the system /var symlink.
        let directory = directory.canonicalize()?;
        let make = |id: &str, kind| Entry {
            id: id.to_owned(),
            mode: 0o755,
            modified_ns: 0,
            link_group: None,
            kind,
        };
        let file = |text: &[u8]| content::put_bytes(text, &directory);
        let mut base = Manifest::from([
            (String::new(), make("root", EntryKind::Directory)),
            ("old".to_owned(), make("dir", EntryKind::Directory)),
            (
                "old/file".to_owned(),
                make("file", file(b"one\ntwo\nthree\nfour\nfive\n")?),
            ),
        ]);
        let mut ours = base.clone();
        let moved_directory = ours.remove("old").unwrap();
        ours.insert("new".into(), moved_directory);
        let mut moved = ours.remove("old/file").unwrap();
        moved.kind = file(b"ONE\ntwo\nthree\nfour\nfive\n")?;
        moved.mode = 0o600;
        ours.insert("new/file".into(), moved);
        let mut theirs = base.clone();
        for entry in theirs.values_mut() {
            entry.modified_ns = 20;
        }
        theirs.get_mut("old/file").unwrap().kind = file(b"one\ntwo\nthree\nfour\nFIVE\n")?;
        theirs.insert("old/added".into(), make("added", file(b"new\n")?));
        let run = |base: &Manifest,
                   ours: &Manifest,
                   theirs: &Manifest,
                   strategy,
                   units: &[Vec<String>]| {
            merge(
                base,
                ours,
                theirs,
                &directory,
                strategy,
                (1, "a"),
                (2, "b"),
                units,
            )
        };
        let merged = run(&base, &ours, &theirs, MergeStrategy::Conflict, &[])?;
        assert!(merged.conflicts.is_empty());
        assert_eq!(merged.manifest["new/file"].mode, 0o600);
        assert_eq!(
            content::read_file(&merged.manifest["new/file"], &directory)?,
            b"ONE\ntwo\nthree\nfour\nFIVE\n"
        );
        assert!(merged.manifest.contains_key("new/added"));
        let units = vec![vec!["old".to_owned()]];
        assert!(
            run(&base, &ours, &theirs, MergeStrategy::Conflict, &units)?
                .conflicts
                .is_empty()
        );
        assert_eq!(
            run(&base, &ours, &theirs, MergeStrategy::Lww, &units)?.manifest,
            theirs
        );
        let deleted = Manifest::from([(String::new(), base[""].clone())]);
        assert!(
            !run(&base, &deleted, &theirs, MergeStrategy::Conflict, &[])?
                .conflicts
                .is_empty()
        );
        assert_eq!(
            run(&base, &deleted, &theirs, MergeStrategy::Lww, &[])?.manifest,
            theirs
        );
        base.insert("other".into(), make("other", EntryKind::Directory));
        let mut left = base.clone();
        let moved_directory = left.remove("old").unwrap();
        left.insert("other/old".into(), moved_directory);
        let moved_file = left.remove("old/file").unwrap();
        left.insert("other/old/file".into(), moved_file);
        let mut right = base.clone();
        for entry in right.values_mut() {
            entry.modified_ns = 20;
        }
        let moved_directory = right.remove("other").unwrap();
        right.insert("old/other".into(), moved_directory);
        let cycle = run(&base, &left, &right, MergeStrategy::Conflict, &[])?;
        assert!(cycle.conflicts.iter().any(|c| c.kind == "directory_cycle"));
        assert_eq!(
            run(&base, &left, &right, MergeStrategy::Lww, &[])?.manifest,
            right
        );
        let mut policy_ours = base.clone();
        let mut policy_theirs = base.clone();
        policy_ours.get_mut("old/file").unwrap().kind = file(b"ours\n")?;
        policy_ours.get_mut("old/file").unwrap().modified_ns = 2;
        policy_theirs.get_mut("old/file").unwrap().kind = file(b"theirs\n")?;
        policy_theirs.get_mut("old/file").unwrap().modified_ns = 3;
        policy_ours.insert(".lww".into(), make("policy", file(b"%include rules\n")?));
        policy_ours.insert("rules".into(), make("rules", file(b"old/**\n")?));
        let selected = run(
            &base,
            &policy_ours,
            &policy_theirs,
            MergeStrategy::Conflict,
            &[],
        )?;
        assert!(selected.conflicts.is_empty());
        assert_eq!(
            content::read_file(&selected.manifest["old/file"], &directory)?,
            b"theirs\n"
        );
        policy_ours.get_mut(".lww").unwrap().kind = file(b"%include rules\n!old/file\n")?;
        assert!(
            !run(
                &base,
                &policy_ours,
                &policy_theirs,
                MergeStrategy::Conflict,
                &[]
            )?
            .conflicts
            .is_empty()
        );
        // Conflict locations must follow identity, not an occupied old path.
        let rename_base = Manifest::from([
            (String::new(), make("root", EntryKind::Directory)),
            ("f".into(), make("original", file(b"base\n")?)),
        ]);
        let mut rename_ours = rename_base.clone();
        let mut moved = rename_ours.remove("f").unwrap();
        moved.kind = file(b"ours\n")?;
        rename_ours.insert("g".into(), moved);
        rename_ours.insert("f".into(), make("replacement", file(b"unrelated\n")?));
        let mut rename_theirs = rename_base.clone();
        rename_theirs.get_mut("f").unwrap().kind = file(b"theirs\n")?;
        let renamed = run(
            &rename_base,
            &rename_ours,
            &rename_theirs,
            MergeStrategy::Conflict,
            &[],
        )?;
        assert_eq!(renamed.conflicts.len(), 1);
        assert_eq!(renamed.conflicts[0].path, "g");
        assert_eq!(renamed.conflicts[0].kind, "content");
        assert_eq!(renamed.manifest["f"], rename_ours["f"]);
        assert_eq!(renamed.manifest["g"].id, "original");
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn hardlink_payload_namespace_and_conflict_closure() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "sprig-merge-links-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&directory)?;
        let directory = directory.canonicalize()?;
        let make = |id: &str, kind, group: Option<&str>| Entry {
            id: id.into(),
            mode: 0o644,
            modified_ns: 1,
            link_group: group.map(str::to_owned),
            kind,
        };
        let bytes = |text: &[u8]| content::put_bytes(text, &directory);
        let base = Manifest::from([
            ("".into(), make("root", EntryKind::Directory, None)),
            (
                "a".into(),
                make("a", bytes(b"one\ntwo\nthree\nfour\nfive\n")?, Some("a")),
            ),
            (
                "b".into(),
                make("b", bytes(b"one\ntwo\nthree\nfour\nfive\n")?, Some("a")),
            ),
        ]);
        let run = |base: &Manifest,
                   ours: &Manifest,
                   theirs: &Manifest,
                   strategy,
                   units: &[Vec<String>]| {
            merge(
                base,
                ours,
                theirs,
                &directory,
                strategy,
                (0, "ours"),
                (0, "theirs"),
                units,
            )
        };
        let edit = |tree: &mut Manifest, text: &[u8], time| -> Result<()> {
            let kind = bytes(text)?;
            for entry in tree
                .values_mut()
                .filter(|entry| entry.link_group.as_deref() == Some("a"))
            {
                entry.kind = kind.clone();
                entry.modified_ns = time;
            }
            Ok(())
        };
        let mut ours = base.clone();
        let mut alias = ours["a"].clone();
        alias.id = "c".into();
        ours.insert("c".into(), alias);
        let mut theirs = base.clone();
        edit(&mut theirs, b"ONE\ntwo\nthree\nfour\nfive\n", 2)?;
        let added = run(&base, &ours, &theirs, MergeStrategy::Conflict, &[])?;
        assert!(added.conflicts.is_empty());
        for path in ["a", "b", "c"] {
            assert_eq!(added.manifest[path].kind, theirs["a"].kind);
            assert_eq!(added.manifest[path].link_group.as_deref(), Some("a"));
        }
        assert_ne!(added.manifest["a"].id, added.manifest["c"].id);
        let mut removed = base.clone();
        removed.remove("b");
        let unlinked = run(&base, &removed, &theirs, MergeStrategy::Conflict, &[])?;
        assert!(unlinked.conflicts.is_empty());
        assert!(!unlinked.manifest.contains_key("b"));
        assert_eq!(unlinked.manifest["a"].kind, theirs["a"].kind);
        // Each inode text merge runs once and applies to every retained alias.
        edit(&mut ours, b"one\ntwo\nthree\nfour\nFIVE\n", 3)?;
        let text = run(&base, &ours, &theirs, MergeStrategy::Conflict, &[])?;
        assert!(text.conflicts.is_empty());
        for path in ["a", "b", "c"] {
            assert_eq!(
                content::read_file(&text.manifest[path], &directory)?,
                b"ONE\ntwo\nthree\nfour\nFIVE\n"
            );
        }
        // Initial singleton -> group uses its existing entry/inode identity.
        let mut singleton = base.clone();
        singleton.remove("b");
        singleton.get_mut("a").unwrap().link_group = None;
        let mut singleton_edit = singleton.clone();
        singleton_edit.get_mut("a").unwrap().kind = bytes(b"singleton edited\n")?;
        let promoted = run(
            &singleton,
            &base,
            &singleton_edit,
            MergeStrategy::Conflict,
            &[],
        )?;
        assert!(promoted.conflicts.is_empty());
        assert_eq!(promoted.manifest["a"].kind, singleton_edit["a"].kind);
        assert_eq!(promoted.manifest["b"].kind, singleton_edit["a"].kind);
        // Different replacements for the same alias are a topology conflict.
        let mut left = base.clone();
        let mut right = base.clone();
        left.get_mut("a").unwrap().link_group = Some("left-inode".into());
        left.get_mut("a").unwrap().kind = bytes(b"left\n")?;
        right.get_mut("a").unwrap().link_group = Some("right-inode".into());
        right.get_mut("a").unwrap().kind = bytes(b"right\n")?;
        right.get_mut("a").unwrap().modified_ns = 5;
        let conflict = run(&base, &left, &right, MergeStrategy::Conflict, &[])?;
        assert!(
            conflict
                .conflicts
                .iter()
                .any(|c| c.kind == "hardlink_topology")
        );
        assert_eq!(conflict.manifest, left);
        content::validate_manifest(&conflict.manifest)?;
        let lww = run(&base, &left, &right, MergeStrategy::Lww, &[])?;
        assert!(lww.conflicts.is_empty());
        assert_eq!(lww.manifest, right);
        // A unit selecting one alias includes its entire inode component.
        let units = vec![vec!["b".into()]];
        let atomic = run(&base, &left, &right, MergeStrategy::Conflict, &units)?;
        assert!(atomic.conflicts.is_empty());
        assert_eq!(atomic.manifest, right);
        // Deleting the last alias still conflicts with a concurrent inode edit.
        let deleted = Manifest::from([("".into(), singleton[""].clone())]);
        let deleted_group = Manifest::from([("".into(), base[""].clone())]);
        assert!(
            !run(&base, &deleted_group, &theirs, MergeStrategy::Conflict, &[])?
                .conflicts
                .is_empty()
        );
        assert!(
            !run(
                &singleton,
                &deleted,
                &singleton_edit,
                MergeStrategy::Conflict,
                &[]
            )?
            .conflicts
            .is_empty()
        );
        // Partial .lww coverage cannot choose inconsistent per-alias payloads.
        let mut policy_left = base.clone();
        let mut policy_right = base.clone();
        edit(&mut policy_left, b"left\n", 2)?;
        edit(&mut policy_right, b"right\n", 4)?;
        policy_left.insert(".lww".into(), make("policy", bytes(b"a\n")?, None));
        let partial = run(
            &base,
            &policy_left,
            &policy_right,
            MergeStrategy::Conflict,
            &[],
        )?;
        assert!(!partial.conflicts.is_empty());
        assert_eq!(partial.manifest["a"].kind, partial.manifest["b"].kind);
        policy_left.get_mut(".lww").unwrap().kind = bytes(b"a\nb\n")?;
        let covered = run(
            &base,
            &policy_left,
            &policy_right,
            MergeStrategy::Conflict,
            &[],
        )?;
        assert!(covered.conflicts.is_empty());
        assert_eq!(covered.manifest["a"].kind, policy_right["a"].kind);
        assert_eq!(covered.manifest["b"].kind, policy_right["b"].kind);
        // A replaced directory entry keeps its ID but points to a new inode.
        // Concurrent writes through the old inode must update only its survivors.
        let mut split = base.clone();
        split.get_mut("a").unwrap().link_group = Some("replacement-inode".into());
        split.get_mut("a").unwrap().kind = bytes(b"replacement bytes\n")?;
        split.get_mut("a").unwrap().modified_ns = 6;
        let split_merged = run(&base, &split, &theirs, MergeStrategy::Conflict, &[])?;
        assert!(split_merged.conflicts.is_empty());
        assert_eq!(split_merged.manifest["a"].id, base["a"].id);
        assert_eq!(
            split_merged.manifest["a"].link_group.as_deref(),
            Some("replacement-inode")
        );
        assert_eq!(split_merged.manifest["a"].kind, split["a"].kind);
        assert_eq!(split_merged.manifest["b"].link_group.as_deref(), Some("a"));
        assert_eq!(split_merged.manifest["b"].kind, theirs["b"].kind);
        // The first/original alias may already be gone: inode identity comes
        // from the persistent group, never the surviving entry's distinct ID.
        let mut survivor_base = base.clone();
        survivor_base.remove("a");
        let mut survivor_ours = survivor_base.clone();
        let mut added_alias = survivor_ours["b"].clone();
        added_alias.id = "survivor-c".into();
        survivor_ours.insert("c".into(), added_alias);
        let mut survivor_theirs = survivor_base.clone();
        edit(&mut survivor_theirs, b"survivor edited\n", 7)?;
        let survivor = run(
            &survivor_base,
            &survivor_ours,
            &survivor_theirs,
            MergeStrategy::Conflict,
            &[],
        )?;
        assert!(survivor.conflicts.is_empty());
        for path in ["b", "c"] {
            assert_eq!(survivor.manifest[path].link_group.as_deref(), Some("a"));
            assert_eq!(survivor.manifest[path].kind, survivor_theirs["b"].kind);
            assert_ne!(survivor.manifest[path].id, "a");
        }
        // The first matching unit owns every alias. A later unit overlapping
        // another alias cannot reverse that choice using an unrelated timestamp.
        let mut unit_base = base.clone();
        unit_base.insert("other".into(), make("other", bytes(b"base other\n")?, None));
        let mut unit_left = unit_base.clone();
        let mut unit_right = unit_base.clone();
        edit(&mut unit_left, b"unit left\n", 2)?;
        edit(&mut unit_right, b"unit right\n", 4)?;
        unit_left.get_mut("other").unwrap().kind = bytes(b"left other\n")?;
        unit_left.get_mut("other").unwrap().modified_ns = 100;
        unit_right.get_mut("other").unwrap().kind = bytes(b"right other\n")?;
        unit_right.get_mut("other").unwrap().modified_ns = 3;
        let overlapping = vec![vec!["a".into()], vec!["b".into(), "other".into()]];
        let selected_units = run(
            &unit_base,
            &unit_left,
            &unit_right,
            MergeStrategy::Conflict,
            &overlapping,
        )?;
        assert!(selected_units.conflicts.is_empty());
        for path in ["a", "b"] {
            assert_eq!(selected_units.manifest[path], unit_right[path]);
        }
        assert_eq!(selected_units.manifest["other"], unit_left["other"]);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
