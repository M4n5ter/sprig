//! Two-phase collection: exact plan, current roots, durable replacement, then reclamation.

use super::*;
use crate::GcResult;

impl Store {
    pub(super) fn gc_fingerprint(
        &self,
        state: &State,
        live: &BTreeSet<String>,
        observations: &observation_roots::ObservationPlan,
    ) -> Result<String> {
        let mut inventory = Vec::new();
        for name in names(&self.objects())? {
            let metadata = platform::symlink_metadata(self.objects().join(&name))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(failure("unexpected object entry during collection"));
            }
            // Index content, not mtimes, identifies the immutable catalog generation.
            let digest = if name == "pack-index" {
                Some(
                    blake3::hash(&fs::read(self.objects().join(&name))?)
                        .to_hex()
                        .to_string(),
                )
            } else {
                None
            };
            inventory.push((name, metadata.len(), digest));
        }
        let roots = self.protected_roots(state)?;
        // The observation plan already contains streaming content evidence for
        // every backup, staged state, current state, and observation file.
        Ok(blake3::hash(&serde_json::to_vec(&(
            "sprig-gc-plan-2",
            state,
            roots,
            live,
            inventory,
            observations,
        ))?)
        .to_hex()
        .to_string())
    }

    /// Applies only the exact still-current plan. All history/working pins remain
    /// protected; compaction may temporarily require another copy of live bytes.
    /// Failure after index publication is not rolled back or blindly retried.
    pub fn gc_apply(&self, plan_id: &str) -> Result<GcResult> {
        self.gc_apply_with_budget(plan_id, 1024 * 1024 * 1024)
    }

    pub fn gc_apply_with_budget(&self, plan_id: &str, max_extra_bytes: u64) -> Result<GcResult> {
        if !valid_hash(plan_id) {
            return Err(failure("invalid collection plan ID"));
        }
        let _lock = self.lock()?;
        let plan = self.gc_locked(&_lock)?;
        if plan.plan_id != plan_id {
            return Err(failure("collection plan is stale; generate a new plan"));
        }
        if plan.compaction_upper_bound_bytes > max_extra_bytes {
            return Err(failure(
                "compaction exceeds the explicit extra-space budget; no changes made",
            ));
        }
        let state = self.state()?;
        let reachable = self.ancestors(&self.protected_roots(&state)?)?;
        let mut live = BTreeSet::new();
        {
            let reader = packed::Reader::open(&self.objects())?;
            for revision in reachable {
                let data = self.revision_with_reader(&revision, &reader)?;
                live.extend(content::referenced_chunks(&data.manifest));
                live.extend(manifest_pages::referenced_chunks(
                    &self.stored_revision(&revision)?.manifest,
                ));
            }
        }
        let observations =
            self.prepare_observation_reclamation(&_lock, &plan.unreachable_observations)?;
        let compacted = packed::compact(&self.objects(), &live)?;
        let reclaimed = packed::reclaim_retired(&self.objects(), &live)?;
        let (observation_files, observation_bytes) =
            self.reclaim_observations(&_lock, observations)?;
        Ok(GcResult {
            plan_id: plan_id.into(),
            compacted_objects: compacted.live_objects,
            compacted_logical_bytes: compacted.logical_bytes,
            retired_pack_bytes: compacted.retired_packs.values().sum(),
            written_pack_bytes: compacted.written_pack_bytes,
            removed_files: reclaimed
                .removed_files
                .checked_add(observation_files)
                .ok_or_else(|| failure("collection count overflow"))?,
            removed_bytes: reclaimed
                .removed_bytes
                .checked_add(observation_bytes)
                .ok_or_else(|| failure("collection size overflow"))?,
            readers_busy: reclaimed.readers_busy,
        })
    }
}

impl Store {
    /// Removing backup roots is a separate, explicit two-phase decision. At
    /// least two newest recovery states remain. No workspace/journal is deleted,
    /// including generations with an uncertain publication outcome.
    pub fn retention(
        &self,
        keep_backups: usize,
        forget_branch_trash: &[String],
        apply: Option<&str>,
    ) -> Result<crate::RetentionPlan> {
        if keep_backups < 2 {
            return Err(failure("retention must keep at least two recovery backups"));
        }
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let mut tokens = forget_branch_trash.to_vec();
        tokens.sort();
        tokens.dedup();
        for token in &tokens {
            let item = state
                .trash
                .get(token)
                .ok_or_else(|| failure("trash token not found"))?;
            if item.workspace.is_some() {
                return Err(failure(
                    "working generations cannot be pruned by metadata retention",
                ));
            }
        }
        // Validate every backup before choosing a prefix to retire.
        let mut backups = Vec::new();
        for name in names(&self.root.join("backups"))? {
            let number = name
                .strip_suffix(".json.zst")
                .ok_or_else(|| failure("invalid backup name"))?
                .parse::<u64>()?;
            let (backup, _): (State, _) = metadata::read(&self.root.join("backups").join(&name))?;
            validate_state(&backup)?;
            backups.push((number, name));
        }
        backups.sort();
        let remove_count = backups.len().saturating_sub(keep_backups);
        let retired: Vec<String> = backups
            .iter()
            .take(remove_count)
            .map(|(_, name)| name.clone())
            .collect();
        // A good older backup must not be discarded merely because a newer
        // state's JSON is structurally valid. Its dependencies must be usable.
        let mut kept_states = vec![state.clone()];
        for (_, name) in backups.iter().skip(remove_count) {
            let (kept, _): (State, _) = metadata::read(&self.root.join("backups").join(name))?;
            kept_states.push(kept);
        }
        let observation_evidence = self.retained_observation_evidence(&_lock, &kept_states)?;
        let mut evidence = Vec::new();
        // Fingerprint every kept backup too: a same-length mutation of one
        // invalidates the plan before any older recovery point can be removed.
        for (_, name) in &backups {
            let (length, digest) =
                self.observation_evidence(&_lock, &self.root.join("backups").join(name))?;
            evidence.push((name, length, digest));
        }
        let plan_id = blake3::hash(&serde_json::to_vec(&(
            "sprig-retention-2",
            &state,
            keep_backups,
            &tokens,
            evidence,
            observation_evidence,
        ))?)
        .to_hex()
        .to_string();
        if let Some(expected) = apply {
            if expected != plan_id {
                return Err(failure("retention plan is stale; generate a new plan"));
            }
            if !tokens.is_empty() {
                for token in &tokens {
                    state.trash.remove(token);
                }
                // Publication precedes retirement. Failure retains all backups;
                // unknown publication requires rereading, not rollback/replay.
                self.save(&_lock, &mut state)?;
            }
            for name in &retired {
                fs::remove_file(self.root.join("backups").join(name))?;
            }
            sync_directory(&self.root.join("backups"))?;
        }
        Ok(crate::RetentionPlan {
            plan_id,
            applied: apply.is_some(),
            generation: state.generation,
            keep_backups,
            backups: retired,
            forgotten_branch_trash: tokens,
        })
    }
}
