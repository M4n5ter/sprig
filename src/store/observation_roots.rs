//! Observation dependencies are recovery roots, never an optional cache.
//!
//! An unknown publication outcome can leave a state in staging. Collection
//! therefore needs every staged state, not merely the currently published one.

use super::*;
use std::io::Read;

#[derive(Serialize)]
pub(super) struct ObservationPlan {
    pub(super) unreachable: Vec<String>,
    pub(super) reclaimable_bytes: u64,
    references: BTreeMap<String, ObservationRef>,
    root_evidence: Vec<(String, u64, String)>,
    inventory: Vec<(String, u64, String)>,
}

pub(super) struct ObservationReclamation {
    candidates: Vec<PathBuf>,
    files: u64,
    bytes: u64,
}

impl Store {
    /// Recovery verifies the selected state separately so that a damaged
    /// current state does not prevent explicitly restoring a good backup.
    pub(super) fn verify_state_observations(&self, lock: &StoreLock, state: &State) -> Result<()> {
        let mut references = BTreeMap::new();
        add_state_references(state, &mut references)?;
        self.verify_observation_references(lock, &references)
    }

    /// Doctor and collection share the complete conservative root definition.
    pub(super) fn verify_observation_roots(&self, lock: &StoreLock, state: &State) -> Result<()> {
        let references = self.observation_references(state)?;
        self.verify_observation_references(lock, &references)
    }

    fn observation_references(&self, state: &State) -> Result<BTreeMap<String, ObservationRef>> {
        let mut references = BTreeMap::new();
        add_state_references(state, &mut references)?;
        // Match historical_states' validation while dropping each decoded
        // metadata state before reading the next. Observation maps are never
        // materialized here, even when many backups share a reference.
        for name in names(&self.root.join("backups"))? {
            if !name.ends_with(".json.zst") {
                return Err(failure(
                    "unknown state backup file; refuse unsafe reachability plan",
                ));
            }
            let (backup, _): (State, _) = metadata::read(&self.root.join("backups").join(name))?;
            add_state_references(&backup, &mut references)?;
        }
        for name in names(&self.root.join("staging"))? {
            let staged = self.staged_observation_state(&name)?;
            add_state_references(&staged, &mut references)?;
        }
        Ok(references)
    }

    pub(super) fn staged_observation_states(&self) -> Result<Vec<State>> {
        let mut states = Vec::new();
        for name in names(&self.root.join("staging"))? {
            states.push(self.staged_observation_state(&name)?);
        }
        Ok(states)
    }

    fn staged_observation_state(&self, name: &str) -> Result<State> {
        if !is_staged_state(name) {
            return Err(failure(
                "unknown staging entry; refuse unsafe observation reachability plan",
            ));
        }
        let path = self.root.join("staging").join(name);
        let staged: State = serde_json::from_reader(platform::open_regular(&path)?)?;
        validate_state(&staged)?;
        Ok(staged)
    }

    fn verify_observation_references(
        &self,
        lock: &StoreLock,
        references: &BTreeMap<String, ObservationRef>,
    ) -> Result<()> {
        for reference in references.values() {
            self.verify_observed(lock, reference)?;
        }
        Ok(())
    }

    /// Retention fingerprints the dependencies of every state that will stay,
    /// not merely the filenames it intends to remove. Shared blobs are verified
    /// and fingerprinted once for the complete retention operation.
    pub(super) fn retained_observation_evidence(
        &self,
        lock: &StoreLock,
        states: &[State],
    ) -> Result<Vec<(ObservationRef, u64, String)>> {
        let mut references = BTreeMap::new();
        for state in states {
            add_state_references(state, &mut references)?;
        }
        self.verify_observation_references(lock, &references)?;
        let mut evidence = Vec::new();
        for reference in references.into_values() {
            let path = self
                .observation_directory()
                .join(format!("{}.zst", reference.hash));
            let (length, digest) = self.observation_evidence(lock, &path)?;
            evidence.push((reference, length, digest));
        }
        Ok(evidence)
    }

    pub(super) fn observation_plan(
        &self,
        lock: &StoreLock,
        state: &State,
    ) -> Result<ObservationPlan> {
        if lock.root != self.root {
            return Err(failure("observation lock belongs to another store"));
        }
        let references = self.observation_references(state)?;
        self.verify_observation_references(lock, &references)?;
        let mut root_evidence = Vec::new();
        let (length, digest) = self.observation_evidence(lock, &self.root.join("state.json"))?;
        root_evidence.push(("state.json".into(), length, digest));
        for area in ["backups", "staging"] {
            for name in names(&self.root.join(area))? {
                let (length, digest) =
                    self.observation_evidence(lock, &self.root.join(area).join(&name))?;
                root_evidence.push((format!("{area}/{name}"), length, digest));
            }
        }
        let mut inventory = Vec::new();
        let mut unreachable = Vec::new();
        let mut reclaimable_bytes = 0_u64;
        for name in names(&self.observation_directory())? {
            let path = self.observation_directory().join(&name);
            // Unknown regular files, including publication temporaries, are
            // fingerprinted but never deleted. Nonregular entries fail closed.
            let (length, digest) = self.observation_evidence(lock, &path)?;
            inventory.push((name.clone(), length, digest));
            let Some(hash) = name.strip_suffix(".zst").filter(|hash| valid_hash(hash)) else {
                continue;
            };
            if !references.contains_key(hash) {
                // A plausible filename alone is not permission to remove a
                // file. Validate canonical payload, frame, and raw identity.
                observations::inspect(
                    &self.observation_directory(),
                    hash,
                    &mut lock.observations.borrow_mut(),
                )?;
                unreachable.push(hash.to_owned());
                reclaimable_bytes = reclaimable_bytes
                    .checked_add(length)
                    .ok_or_else(|| failure("observation collection size overflow"))?;
            }
        }
        Ok(ObservationPlan {
            unreachable,
            reclaimable_bytes,
            references,
            root_evidence,
            inventory,
        })
    }

    /// Streaming content evidence detects replacement even when length and
    /// timestamps are unchanged. All reads spend the operation's shared budget.
    pub(super) fn observation_evidence(
        &self,
        lock: &StoreLock,
        path: &Path,
    ) -> Result<(u64, String)> {
        if lock.root != self.root {
            return Err(failure("observation lock belongs to another store"));
        }
        let mut file = platform::open_regular(path)?;
        let mut hash = blake3::Hasher::new();
        let mut length = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            lock.observations
                .borrow_mut()
                .charge_encoded(count as u64)?;
            hash.update(&buffer[..count]);
            length = length
                .checked_add(count as u64)
                .ok_or_else(|| failure("observation inventory size overflow"))?;
        }
        Ok((length, hash.finalize().to_hex().to_string()))
    }

    /// Called only with the candidates from a freshly regenerated exact plan
    /// while its StoreLock is still held. No workspace publication can interpose.
    pub(super) fn prepare_observation_reclamation(
        &self,
        lock: &StoreLock,
        hashes: &[String],
    ) -> Result<ObservationReclamation> {
        if lock.root != self.root {
            return Err(failure("observation lock belongs to another store"));
        }
        let directory = self.observation_directory();
        let mut files = 0_u64;
        let mut bytes = 0_u64;
        // Verify the complete deletion batch before deleting its first entry.
        // Exhausting a budget therefore cannot cause a partially verified sweep.
        let mut verified = BTreeMap::new();
        for hash in hashes {
            if !valid_hash(hash) || verified.contains_key(hash) {
                return Err(failure("invalid observation collection candidate"));
            }
            observations::inspect(&directory, hash, &mut lock.observations.borrow_mut())?;
            let path = directory.join(format!("{hash}.zst"));
            let length = platform::open_regular(&path)?.metadata()?.len();
            verified.insert(hash, (path, length));
        }
        let mut candidates = Vec::new();
        for (path, length) in verified.into_values() {
            files = files
                .checked_add(1)
                .ok_or_else(|| failure("observation collection count overflow"))?;
            bytes = bytes
                .checked_add(length)
                .ok_or_else(|| failure("observation collection size overflow"))?;
            candidates.push(path);
        }
        Ok(ObservationReclamation {
            candidates,
            files,
            bytes,
        })
    }

    pub(super) fn reclaim_observations(
        &self,
        lock: &StoreLock,
        prepared: ObservationReclamation,
    ) -> Result<(u64, u64)> {
        if lock.root != self.root {
            return Err(failure("observation lock belongs to another store"));
        }
        for path in prepared.candidates {
            super::fault("before_observation_unlink")?;
            fs::remove_file(path)?;
        }
        if prepared.files != 0 {
            super::fault("before_observation_gc_directory_sync")?;
            sync_directory(&self.observation_directory())?;
        }
        Ok((prepared.files, prepared.bytes))
    }
}

fn is_staged_state(name: &str) -> bool {
    ["state-", "recovered-state-", "retained-state-"]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|name| name.strip_suffix(".json"))
                .is_some_and(|id| !id.is_empty())
        })
}

fn add_state_references(
    state: &State,
    references: &mut BTreeMap<String, ObservationRef>,
) -> Result<()> {
    validate_state(state)?;
    for workspace in state.workspaces.values().chain(
        state
            .trash
            .values()
            .filter_map(|item| item.workspace.as_ref()),
    ) {
        let reference = &workspace.observed;
        observations::validate_descriptor(reference)?;
        if let Some(previous) = references.get(&reference.hash) {
            if serde_json::to_vec(previous)? != serde_json::to_vec(reference)? {
                return Err(failure("conflicting observation descriptors for one hash"));
            }
        } else {
            references.insert(reference.hash.clone(), reference.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        store: Store,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let root = std::env::temp_dir().join(format!("sprig-observation-roots-{}", new_id()?));
            fs::create_dir(&root)?;
            let root = root.canonicalize()?;
            let source = root.join("source");
            fs::create_dir(&source)?;
            fs::write(source.join("file"), b"retained content")?;
            let store = Store::init(&root.join("store"))?;
            store.import(&source, "main", CopyMode::Copy)?;
            Ok(Self { root, store })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
        #[cfg(unix)]
        platform::set_path_mode(path, 0o600)?;
        fs::write(path, bytes)?;
        Ok(())
    }

    #[test]
    fn same_length_inventory_tamper_stales_plan_and_unknown_files_survive() -> Result<()> {
        let fixture = Fixture::new()?;
        let store = &fixture.store;
        let temporary = store.observation_directory().join(".tmp-unpublished");
        let unknown = store.observation_directory().join("keep-this-note");
        fs::write(&temporary, b"first")?;
        fs::write(&unknown, b"do not remove")?;
        let plan = store.gc()?;
        let state_before = fs::read(store.root.join("state.json"))?;
        fs::write(&temporary, b"other")?; // Same length, different content.
        assert!(store.gc_apply(&plan.plan_id).is_err());
        assert_eq!(fs::read(store.root.join("state.json"))?, state_before);
        let replacement = store.gc()?;
        assert_ne!(replacement.plan_id, plan.plan_id);
        store.gc_apply(&replacement.plan_id)?;
        assert_eq!(fs::read(&temporary)?, b"other");
        assert_eq!(fs::read(&unknown)?, b"do not remove");
        assert!(store.doctor()?.healthy);
        Ok(())
    }

    #[test]
    fn retained_backup_evidence_and_dependencies_protect_older_good_backups() -> Result<()> {
        let fixture = Fixture::new()?;
        let store = &fixture.store;
        for index in 0..4 {
            store.branch(&format!("cold-{index}"), "main")?;
        }
        let unique = {
            let lock = store.lock()?;
            let state = store.state()?;
            let mut observed = store.read_observed(&lock, &state.workspaces["main"].observed)?;
            observed.values_mut().next().unwrap().inode ^= 1;
            store.put_observed(&lock, &observed)?
        };
        let directory = store.root.join("backups");
        let name = names(&directory)?
            .into_iter()
            .max_by_key(|name| {
                name.strip_suffix(".json.zst")
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
            })
            .unwrap();
        let kept_path = directory.join(name);
        let (mut kept, _): (State, _) = metadata::read(&kept_path)?;
        kept.workspaces.get_mut("main").unwrap().observed = unique.clone();
        let mut kept_bytes = serde_json::to_vec(&kept)?;
        replace_file(&kept_path, &metadata::encode(&kept_bytes)?)?;
        let first = store.retention(2, &[], None)?;
        assert!(!first.backups.is_empty());
        // Semantically identical but different kept evidence must stale an
        // exact plan, even though none of the removal candidates changed.
        kept_bytes.push(b'\n');
        replace_file(&kept_path, &metadata::encode(&kept_bytes)?)?;
        assert!(store.retention(2, &[], Some(&first.plan_id)).is_err());
        let current = store.retention(2, &[], None)?;
        assert_ne!(first.plan_id, current.plan_id);
        let evidence_before: Vec<_> = names(&directory)?
            .into_iter()
            .map(|name| Ok((name.clone(), fs::read(directory.join(name))?)))
            .collect::<Result<_>>()?;
        let blob = store
            .observation_directory()
            .join(format!("{}.zst", unique.hash));
        let length = fs::metadata(&blob)?.len() as usize;
        replace_file(&blob, &vec![b'x'; length])?;
        assert!(store.retention(2, &[], None).is_err());
        assert!(store.retention(2, &[], Some(&current.plan_id)).is_err());
        for (name, bytes) in evidence_before {
            assert_eq!(fs::read(directory.join(name))?, bytes);
        }
        Ok(())
    }
}
