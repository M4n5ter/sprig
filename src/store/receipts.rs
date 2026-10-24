//! Internal bounded publication ledger. No automatic expiry, eviction or pruning.
use super::*;
use crate::model::{OperationReceipt, OperationRequest, OperationResult};

pub(super) const RECEIPT_LIMIT: usize = 256;

fn validate_request(r: &OperationRequest) -> Result<()> {
    validate_name(&r.name)?;
    if !valid_hash(&r.nonce)
        || !valid_hash(&r.candidate_revision)
        || !valid_hash(&r.request_digest)
        || r.expected_generation == u64::MAX
    {
        return Err(failure("invalid snapshot operation request"));
    }
    Ok(())
}

pub(super) fn validate_receipts(state: &State) -> Result<()> {
    if state.operation_receipts.len() > RECEIPT_LIMIT {
        return Err(failure("operation receipt capacity exceeded"));
    }
    for (nonce, receipt) in &state.operation_receipts {
        validate_request(&receipt.request)?;
        if *nonce != receipt.request.nonce
            || receipt.reservation_generation != receipt.request.expected_generation + 1
            || receipt.reservation_generation > state.generation
        {
            return Err(failure("invalid operation receipt binding/generation"));
        }
        match &receipt.result {
            OperationResult::Reserved => {}
            OperationResult::Committed {
                publication_generation,
                directory,
            } => {
                validate_name(directory)?;
                if *publication_generation <= receipt.reservation_generation
                    || *publication_generation > state.generation
                {
                    return Err(failure("invalid operation publication generation"));
                }
            }
            OperationResult::Aborted { reason }
                if reason == "state_changed" || reason == "cancelled" => {}
            OperationResult::Aborted { .. } => {
                return Err(failure("invalid operation abort reason"));
            }
        }
    }
    Ok(())
}

fn existing(state: &State, request: &OperationRequest) -> Result<Option<OperationReceipt>> {
    validate_request(request)?;
    if let Some(receipt) = state.operation_receipts.get(&request.nonce) {
        if receipt.request != *request {
            return Err(failure("operation nonce already binds a different request"));
        }
        if state.receipt_recovery_blocked && receipt.result == OperationResult::Reserved {
            return Err(failure(
                "reserved operation outcome unknown after damaged-state recovery",
            ));
        }
        return Ok(Some(receipt.clone()));
    }
    if state.receipt_recovery_blocked {
        return Err(failure(
            "operation outcome unknown after damaged-state recovery",
        ));
    }
    Ok(None)
}

// Deliberately crate-private, not yet wired to the experimental VSS CLI. The
// adapter must bind all source/capture options into the canonical request digest.
#[allow(dead_code)]
impl Store {
    // Observing the renamed file is not a durable acknowledgement. Retry the
    // checked publication barriers while the metadata lock prevents replacement.
    fn acknowledge_snapshot_state(&self) -> Result<()> {
        #[cfg(test)]
        fail_save(SaveFault::Acknowledge)?;
        platform::open_read_write(&self.root.join("state.json"))?.sync_all()?;
        sync_directory(&self.root)?;
        sync_directory(&self.root.join("staging"))?;
        Ok(())
    }

    fn validate_snapshot_candidate(&self, candidate: &str) -> Result<()> {
        let reader = packed::Reader::open(&self.objects())?;
        let mut chunks = BTreeSet::new();
        for id in self.ancestors(&[candidate.to_owned()])? {
            let revision = self.revision_with_reader(&id, &reader)?;
            chunks.extend(content::referenced_chunks(&revision.manifest));
        }
        for hash in chunks {
            reader.read(&hash)?;
        }
        Ok(())
    }

    pub(crate) fn reserve_snapshot_operation(
        &self,
        request: &OperationRequest,
    ) -> Result<OperationReceipt> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        if let Some(receipt) = existing(&state, request)? {
            self.acknowledge_snapshot_state()?;
            return Ok(receipt);
        }
        if state.operation_receipts.len() == RECEIPT_LIMIT {
            return Err(failure("operation receipt capacity full; nothing evicted"));
        }
        if state.generation != request.expected_generation {
            return Err(failure(
                "operation expected metadata generation does not match",
            ));
        }
        fresh_name(&state, &request.name, true)?;
        self.validate_snapshot_candidate(&request.candidate_revision)?;
        let receipt = OperationReceipt {
            request: request.clone(),
            reservation_generation: state.generation + 1,
            result: OperationResult::Reserved,
        };
        state
            .operation_receipts
            .insert(request.nonce.clone(), receipt.clone());
        self.save(&mut state)?;
        Ok(receipt)
    }

    /// Historical publication fact, not a live workspace/path availability claim.
    pub(crate) fn snapshot_operation(
        &self,
        request: &OperationRequest,
    ) -> Result<Option<OperationReceipt>> {
        let _lock = self.lock()?;
        let result = existing(&self.state()?, request)?;
        self.acknowledge_snapshot_state()?;
        Ok(result)
    }

    pub(crate) fn publish_snapshot_operation(
        &self,
        request: &OperationRequest,
    ) -> Result<OperationReceipt> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let mut receipt =
            existing(&state, request)?.ok_or_else(|| failure("operation not reserved"))?;
        if receipt.result != OperationResult::Reserved {
            self.acknowledge_snapshot_state()?;
            return Ok(receipt);
        }
        if state.generation != receipt.reservation_generation {
            receipt.result = OperationResult::Aborted {
                reason: "state_changed".into(),
            };
        } else {
            fresh_name(&state, &request.name, true)?;
            self.validate_snapshot_candidate(&request.candidate_revision)?;
            let (directory, observed, backend) =
                self.writable_generation(&request.candidate_revision)?;
            let workspace = Workspace {
                backend,
                directory: directory.clone(),
                observed,
                branch: request.name.clone(),
                base_revision: request.candidate_revision.clone(),
                observation_epoch: None,
                pending: None,
            };
            state
                .branches
                .insert(request.name.clone(), request.candidate_revision.clone());
            state.workspaces.insert(request.name.clone(), workspace);
            receipt.result = OperationResult::Committed {
                publication_generation: state
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| failure("metadata generation exhausted"))?,
                directory,
            };
        }
        state
            .operation_receipts
            .insert(request.nonce.clone(), receipt.clone());
        self.save(&mut state)?;
        Ok(receipt)
    }

    pub(crate) fn cancel_snapshot_operation(
        &self,
        request: &OperationRequest,
    ) -> Result<OperationReceipt> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let mut receipt =
            existing(&state, request)?.ok_or_else(|| failure("operation not reserved"))?;
        if receipt.result != OperationResult::Reserved {
            self.acknowledge_snapshot_state()?;
            return Ok(receipt);
        }
        receipt.result = OperationResult::Aborted {
            reason: "cancelled".into(),
        };
        state
            .operation_receipts
            .insert(request.nonce.clone(), receipt.clone());
        self.save(&mut state)?;
        Ok(receipt)
    }
}

impl Store {
    pub(super) fn diagnose_receipt_generations(&self, state: &State) -> Result<Vec<String>> {
        let mut states = self.historical_states()?;
        states.push(state.clone());
        let mut checked = BTreeSet::new();
        let mut errors = Vec::new();
        for state in states {
            for receipt in state.operation_receipts.values() {
                let OperationResult::Committed { directory, .. } = &receipt.result else {
                    continue;
                };
                if !checked.insert((
                    directory.clone(),
                    receipt.request.candidate_revision.clone(),
                )) {
                    continue;
                }
                let result = (|| -> Result<()> {
                    let generation = self.root.join("native").join(directory);
                    require_directory(&generation)?;
                    require_directory(&generation.join("tree"))?;
                    let pin = generation.join("writable-base");
                    require_file(&pin)?;
                    if fs::read_to_string(pin)? != receipt.request.candidate_revision {
                        return Err(failure("receipt generation revision pin mismatch"));
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    errors.push(format!("receipt generation {directory}: {error}"));
                }
            }
        }
        Ok(errors)
    }

    /// Explicit metadata rollback must not roll back known publication facts.
    pub(super) fn merge_recovery_receipts(&self, restored: &mut State) -> Result<()> {
        let mut states = self.historical_states()?;
        match self.state() {
            Ok(current) => states.push(current),
            Err(_) => restored.receipt_recovery_blocked = true,
        }
        states.sort_by_key(|state| state.generation);
        for state in states {
            restored.receipt_recovery_blocked |= state.receipt_recovery_blocked;
            for (nonce, incoming) in state.operation_receipts {
                if let Some(previous) = restored.operation_receipts.get(&nonce) {
                    if previous.request != incoming.request
                        || previous.reservation_generation != incoming.reservation_generation
                        || (previous.result != OperationResult::Reserved
                            && incoming.result != OperationResult::Reserved
                            && previous.result != incoming.result)
                    {
                        return Err(failure(
                            "inconsistent operation receipts across recovery states",
                        ));
                    }
                    if incoming.result == OperationResult::Reserved {
                        continue;
                    }
                }
                restored.operation_receipts.insert(nonce, incoming);
            }
        }
        if restored.operation_receipts.len() > RECEIPT_LIMIT {
            return Err(failure(
                "recovery receipt union exceeds capacity; nothing discarded",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum SaveFault {
    BeforeRename,
    AfterRename,
    Acknowledge,
}
#[cfg(test)]
thread_local! {
    static SAVE_FAULT: std::cell::Cell<Option<SaveFault>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
pub(super) fn fail_save(phase: SaveFault) -> Result<()> {
    if SAVE_FAULT.with(|fault| fault.get() == Some(phase)) {
        return Err(failure("injected state publication failure"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Fixture {
        path: PathBuf,
        store: Store,
        candidate: String,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("sprig-receipt-{}", new_id().unwrap()));
            fs::create_dir(&path).unwrap();
            let path = path.canonicalize().unwrap();
            let store = Store::init(&path.join("store")).unwrap();
            let mut batch = content::ObjectBatch::open(&store.objects()).unwrap();
            let kind = batch
                .put_reader(&mut Cursor::new(vec![b'x'; 65536]))
                .unwrap();
            batch.finish().unwrap();
            let manifest = Manifest::from([
                (
                    String::new(),
                    content::Entry {
                        id: "root".into(),
                        link_group: None,
                        mode: 0o755,
                        modified_ns: 0,
                        kind: EntryKind::Directory,
                    },
                ),
                (
                    "file".into(),
                    content::Entry {
                        id: "file".into(),
                        link_group: None,
                        mode: 0o644,
                        modified_ns: 0,
                        kind,
                    },
                ),
            ]);
            let candidate = store
                .put_revision(&Revision {
                    parents: vec![],
                    clock: 1,
                    message: "candidate".into(),
                    manifest,
                })
                .unwrap();
            Self {
                path,
                store,
                candidate,
            }
        }
        fn request(&self, n: u64) -> OperationRequest {
            OperationRequest {
                nonce: format!("{n:064x}"),
                name: format!("snapshot-{n}"),
                expected_generation: self.store.state().unwrap().generation,
                candidate_revision: self.candidate.clone(),
                request_digest: "d".repeat(64),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }
    fn fault<T>(phase: SaveFault, action: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SAVE_FAULT.with(|f| f.set(None));
            }
        }
        SAVE_FAULT.with(|f| f.set(Some(phase)));
        let _reset = Reset;
        action()
    }

    #[test]
    fn missing_receipt_generation_is_diagnosed_without_republishing() {
        let f = Fixture::new();
        let r = f.request(1);
        f.store.reserve_snapshot_operation(&r).unwrap();
        let committed = f.store.publish_snapshot_operation(&r).unwrap();
        let OperationResult::Committed { directory, .. } = &committed.result else {
            panic!()
        };
        {
            let _lock = f.store.lock().unwrap();
            let mut state = f.store.state().unwrap();
            state.branches.clear();
            state.workspaces.clear();
            f.store.save(&mut state).unwrap();
        }
        fs::remove_dir_all(f.store.root.join("native").join(directory)).unwrap();
        assert!(
            f.store
                .doctor()
                .unwrap()
                .errors
                .iter()
                .any(|e| e.contains("receipt generation"))
        );
        assert_eq!(f.store.publish_snapshot_operation(&r).unwrap(), committed);
        assert!(!f.store.root.join("native").join(directory).exists());
    }

    #[test]
    fn missing_candidate_payload_never_gets_a_receipt() {
        let f = Fixture::new();
        let r = f.request(1);
        let plan = f.store.gc().unwrap();
        assert!(!plan.unreachable_objects.is_empty());
        f.store.gc_apply(&plan.plan_id).unwrap();
        assert!(f.store.reserve_snapshot_operation(&r).is_err());
        assert!(f.store.snapshot_operation(&r).unwrap().is_none());
        assert!(f.store.state().unwrap().branches.is_empty());
    }

    #[test]
    fn concurrent_retries_publish_once_and_cancel_acknowledgements_are_exact() {
        let f = Fixture::new();
        let r = f.request(1);
        f.store.reserve_snapshot_operation(&r).unwrap();
        let barrier = std::sync::Barrier::new(3);
        let outcomes = std::thread::scope(|scope| {
            let one = scope.spawn(|| {
                barrier.wait();
                f.store.publish_snapshot_operation(&r)
            });
            let two = scope.spawn(|| {
                barrier.wait();
                f.store.publish_snapshot_operation(&r)
            });
            barrier.wait();
            [one.join().unwrap(), two.join().unwrap()]
        });
        let committed = f.store.publish_snapshot_operation(&r).unwrap();
        for outcome in outcomes {
            match outcome {
                Ok(receipt) => assert_eq!(receipt, committed),
                Err(error) => assert!(error.to_string().contains("store_locked")),
            }
        }
        let state = f.store.state().unwrap();
        assert_eq!(state.generation, 2);
        assert_eq!(state.branches.len(), 1);
        assert_eq!(state.workspaces.len(), 1);
        for phase in [SaveFault::BeforeRename, SaveFault::AfterRename] {
            let r = f.request(if phase == SaveFault::BeforeRename {
                2
            } else {
                3
            });
            f.store.reserve_snapshot_operation(&r).unwrap();
            assert!(fault(phase, || f.store.cancel_snapshot_operation(&r)).is_err());
            let seen = f.store.snapshot_operation(&r).unwrap().unwrap();
            assert_eq!(
                matches!(seen.result, OperationResult::Aborted { .. }),
                phase == SaveFault::AfterRename
            );
            let cancelled = f.store.cancel_snapshot_operation(&r).unwrap();
            assert_eq!(
                cancelled.result,
                OperationResult::Aborted {
                    reason: "cancelled".into()
                }
            );
            assert_eq!(f.store.publish_snapshot_operation(&r).unwrap(), cancelled);
        }
        let mut encoded = serde_json::to_value(f.store.state().unwrap()).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("operation_receipts");
        assert!(serde_json::from_value::<State>(encoded).is_err());
    }

    #[test]
    fn receipts_resolve_acknowledgement_gaps_and_reject_nonce_rebinding() {
        for phase in [SaveFault::BeforeRename, SaveFault::AfterRename] {
            let f = Fixture::new();
            let r = f.request(1);
            assert!(fault(phase, || f.store.reserve_snapshot_operation(&r)).is_err());
            assert_eq!(
                f.store.snapshot_operation(&r).unwrap().is_some(),
                phase == SaveFault::AfterRename
            );
            let reservation = f.store.reserve_snapshot_operation(&r).unwrap();
            assert_eq!(reservation.result, OperationResult::Reserved);
            assert!(fault(phase, || f.store.publish_snapshot_operation(&r)).is_err());
            if phase == SaveFault::AfterRename {
                assert!(fault(SaveFault::Acknowledge, || f.store.snapshot_operation(&r)).is_err());
                assert!(
                    fault(SaveFault::Acknowledge, || f
                        .store
                        .publish_snapshot_operation(&r))
                    .is_err()
                );
                assert!(
                    fault(SaveFault::Acknowledge, || f
                        .store
                        .cancel_snapshot_operation(&r))
                    .is_err()
                );
                assert!(
                    fault(SaveFault::Acknowledge, || f
                        .store
                        .reserve_snapshot_operation(&r))
                    .is_err()
                );
            }
            let seen = f.store.snapshot_operation(&r).unwrap().unwrap();
            assert_eq!(
                matches!(seen.result, OperationResult::Committed { .. }),
                phase == SaveFault::AfterRename
            );
            let published = f.store.publish_snapshot_operation(&r).unwrap();
            assert!(matches!(
                published.result,
                OperationResult::Committed { .. }
            ));
            let generation = f.store.state().unwrap().generation;
            assert_eq!(f.store.publish_snapshot_operation(&r).unwrap(), published);
            assert_eq!(f.store.cancel_snapshot_operation(&r).unwrap(), published);
            assert_eq!(f.store.state().unwrap().generation, generation);
            for changed in [
                OperationRequest {
                    name: "other".into(),
                    ..r.clone()
                },
                OperationRequest {
                    request_digest: "e".repeat(64),
                    ..r.clone()
                },
                OperationRequest {
                    candidate_revision: "f".repeat(64),
                    ..r.clone()
                },
                OperationRequest {
                    expected_generation: r.expected_generation + 1,
                    ..r.clone()
                },
            ] {
                assert!(f.store.reserve_snapshot_operation(&changed).is_err());
                assert!(f.store.publish_snapshot_operation(&changed).is_err());
                assert!(f.store.snapshot_operation(&changed).is_err());
            }
        }
    }

    #[test]
    fn receipts_survive_successor_heads_and_protect_gc_and_generation() {
        let f = Fixture::new();
        let r = f.request(1);
        f.store.reserve_snapshot_operation(&r).unwrap();
        let committed = f.store.publish_snapshot_operation(&r).unwrap();
        let directory = match &committed.result {
            OperationResult::Committed { directory, .. } => directory.clone(),
            _ => unreachable!(),
        };
        // An unrelated successful state publication removes the current names;
        // receipt lookup still returns history, not a supposedly live pathname.
        {
            let _lock = f.store.lock().unwrap();
            let mut state = f.store.state().unwrap();
            state.branches.clear();
            state.workspaces.clear();
            f.store.save(&mut state).unwrap();
            f.store.save(&mut state).unwrap();
            f.store.save(&mut state).unwrap();
        }
        assert_eq!(
            f.store.snapshot_operation(&r).unwrap(),
            Some(committed.clone())
        );
        assert_eq!(f.store.publish_snapshot_operation(&r).unwrap(), committed);
        assert!(f.store.state().unwrap().workspaces.is_empty());
        let retention = f.store.retention(2, &[], None).unwrap();
        f.store.retention(2, &[], Some(&retention.plan_id)).unwrap();
        let state = f.store.state().unwrap();
        assert!(
            f.store
                .protected_directories(&state)
                .unwrap()
                .contains(&directory)
        );
        let plan = f.store.gc().unwrap();
        assert!(!plan.unreachable_revisions.contains(&f.candidate));
        assert!(!plan.orphan_workspaces.contains(&directory));
        f.store.gc_apply(&plan.plan_id).unwrap();
        let reader = packed::Reader::open(&f.store.objects()).unwrap();
        for hash in content::referenced_chunks(&f.store.revision(&f.candidate).unwrap().manifest) {
            reader.read(&hash).unwrap();
        }
        drop(reader);
        assert_eq!(f.store.snapshot_operation(&r).unwrap(), Some(committed));
    }

    #[test]
    fn reservations_conflict_cancel_and_capacity_without_eviction() {
        let f = Fixture::new();
        let r = f.request(1);
        f.store.reserve_snapshot_operation(&r).unwrap();
        let next = f.request(2);
        f.store.reserve_snapshot_operation(&next).unwrap();
        let aborted = f.store.publish_snapshot_operation(&r).unwrap();
        assert_eq!(
            aborted.result,
            OperationResult::Aborted {
                reason: "state_changed".into()
            }
        );
        assert_eq!(
            f.store.cancel_snapshot_operation(&next).unwrap().result,
            OperationResult::Aborted {
                reason: "cancelled".into()
            }
        );
        assert!(f.store.state().unwrap().branches.is_empty());
        let _lock = f.store.lock().unwrap();
        let mut state = f.store.state().unwrap();
        for n in 3..=RECEIPT_LIMIT as u64 {
            let request = OperationRequest {
                nonce: format!("{n:064x}"),
                name: format!("snapshot-{n}"),
                ..r.clone()
            };
            state.operation_receipts.insert(
                request.nonce.clone(),
                OperationReceipt {
                    request,
                    reservation_generation: 1,
                    result: OperationResult::Aborted {
                        reason: "cancelled".into(),
                    },
                },
            );
        }
        f.store.save(&mut state).unwrap();
        drop(_lock);
        assert!(f.store.reserve_snapshot_operation(&f.request(999)).is_err());
        assert_eq!(f.store.snapshot_operation(&r).unwrap(), Some(aborted));
        assert_eq!(
            f.store.state().unwrap().operation_receipts.len(),
            RECEIPT_LIMIT
        );
    }

    #[test]
    fn recovery_merges_terminal_facts_and_blocks_damaged_latest_state() {
        let f = Fixture::new();
        let r = f.request(1);
        f.store.reserve_snapshot_operation(&r).unwrap();
        let committed = f.store.publish_snapshot_operation(&r).unwrap();
        Store::recover_state(&f.store.root, 0).unwrap();
        assert_eq!(
            f.store.snapshot_operation(&r).unwrap(),
            Some(committed.clone())
        );
        assert_eq!(f.store.publish_snapshot_operation(&r).unwrap(), committed);
        assert!(f.store.state().unwrap().branches.is_empty());
        // A current state containing an unknown newer receipt may be lost.
        let pending = f.request(2);
        f.store.reserve_snapshot_operation(&pending).unwrap();
        {
            let _lock = f.store.lock().unwrap();
            let mut state = f.store.state().unwrap();
            f.store.save(&mut state).unwrap();
        }
        fs::write(f.store.root.join("state.json"), b"damaged synthetic state").unwrap();
        Store::recover_state(&f.store.root, 0).unwrap();
        assert!(f.store.state().unwrap().receipt_recovery_blocked);
        assert!(f.store.snapshot_operation(&pending).is_err());
        assert!(f.store.publish_snapshot_operation(&pending).is_err());
        assert!(f.store.cancel_snapshot_operation(&pending).is_err());
        assert!(f.store.reserve_snapshot_operation(&f.request(3)).is_err());
        assert_eq!(f.store.snapshot_operation(&r).unwrap(), Some(committed));
        Store::recover_state(&f.store.root, 0).unwrap();
        assert!(f.store.state().unwrap().receipt_recovery_blocked);
        // Well-formed but contradictory terminal histories must not be guessed.
        let mut conflicting = f.store.state().unwrap();
        conflicting
            .operation_receipts
            .get_mut(&r.nonce)
            .unwrap()
            .result = OperationResult::Aborted {
            reason: "cancelled".into(),
        };
        assert!(f.store.merge_recovery_receipts(&mut conflicting).is_err());
    }
}
