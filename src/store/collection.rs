//! Two-phase collection: exact plan, current roots, durable replacement, then reclamation.

use super::*;
use crate::GcResult;

impl Store {
    pub(super) fn gc_fingerprint(&self, state: &State, live: &BTreeSet<String>) -> Result<String> {
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
        let mut backups = Vec::new();
        for name in names(&self.root.join("backups"))? {
            backups.push((
                name.clone(),
                blake3::hash(&fs::read(self.root.join("backups").join(name))?)
                    .to_hex()
                    .to_string(),
            ));
        }
        Ok(blake3::hash(&serde_json::to_vec(&(
            "sprig-gc-plan-1",
            state,
            roots,
            backups,
            live,
            inventory,
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
        let plan = self.gc_locked()?;
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
        let compacted = packed::compact(&self.objects(), &live)?;
        let reclaimed = packed::reclaim_retired(&self.objects(), &live)?;
        Ok(GcResult {
            plan_id: plan_id.into(),
            compacted_objects: compacted.live_objects,
            compacted_logical_bytes: compacted.logical_bytes,
            retired_pack_bytes: compacted.retired_packs.values().sum(),
            written_pack_bytes: compacted.written_pack_bytes,
            removed_files: reclaimed.removed_files,
            removed_bytes: reclaimed.removed_bytes,
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
        self.historical_states()?;
        let mut backups = Vec::new();
        for name in names(&self.root.join("backups"))? {
            let number = name
                .strip_suffix(".json.zst")
                .ok_or_else(|| failure("invalid backup name"))?
                .parse::<u64>()?;
            backups.push((number, name));
        }
        backups.sort();
        let remove_count = backups.len().saturating_sub(keep_backups);
        let backups: Vec<String> = backups
            .into_iter()
            .take(remove_count)
            .map(|(_, name)| name)
            .collect();
        let mut evidence = Vec::new();
        for name in &backups {
            evidence.push((
                name,
                blake3::hash(&fs::read(self.root.join("backups").join(name))?)
                    .to_hex()
                    .to_string(),
            ));
        }
        let plan_id = blake3::hash(&serde_json::to_vec(&(
            "sprig-retention-1",
            &state,
            keep_backups,
            &tokens,
            evidence,
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
                self.save(&mut state)?;
            }
            for name in &backups {
                fs::remove_file(self.root.join("backups").join(name))?;
            }
            sync_directory(&self.root.join("backups"))?;
        }
        Ok(crate::RetentionPlan {
            plan_id,
            applied: apply.is_some(),
            generation: state.generation,
            keep_backups,
            backups,
            forgotten_branch_trash: tokens,
        })
    }
}
