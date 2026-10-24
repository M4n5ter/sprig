# Atomic Store operation receipts (experimental)

This is a Store-only building block for an online **storage snapshot**, not an
production online snapshot command or VSS requester. The opt-in synthetic VSS
Cargo fixture connects this primitive through hidden experimental CLI commands. Strict checkpoint is unchanged. No
Windows service, volume, privilege, or driver operation is added.

## Internal sequence

1. The adapter holds its separately established source snapshot lease and produces
   a sealed candidate revision. Before reservation, that candidate is not pinned
   by this protocol: collection can make reservation fail safely.
2. Construct `OperationRequest`: Store identity, nonce, new branch/workspace name, expected metadata
   generation, candidate revision, and a canonical digest of **all** adapter source
   identity, snapshot identity, capture options and result-affecting policy. These
   values are immutable across retries. The digest is not a lease certificate.
3. `reserve_snapshot_operation` validates the candidate and all ancestors/payloads
   under the Store lock, then atomically records `Reserved`. The receipt pins the
   candidate and ancestors. A competing metadata operation may make publication
   abort conservatively; it cannot silently rebase the request.
4. `publish_snapshot_operation` revalidates content, prepares a private owned
   logical generation, and atomically records the branch, workspace and exact
   `Committed { publication_generation, directory }` receipt in one state save.
   `state_changed` and explicit cancellation are terminal `Aborted` receipts.
5. After any acknowledgement error, query the **same complete request**. Every
   query/replay first completes checked state-file and directory synchronization.
   A failed barrier is an error, not a confirmed result. A committed receipt is a
   historical publication fact; it does not promise the named workspace is still
   present, unchanged, or mounted. Do not republish it if a later operation moved
   the head or removed the workspace name.

The prior requester lifecycle's `StoreOutcome.Unknown` maps to errors or blocked
recovery, not to `Aborted`. Only the exact terminal receipt may settle publication.
A VSS cleanup result remains separate: committed Store state cannot prove the
snapshot was deleted, and failed snapshot cleanup cannot undo a Store commit.
The synthetic requester retains its lease through prepare and the complete child
Job. The caller saves the full request before commit and may query it without a
snapshot handle. This is a trusted-controller protocol, not an authorization
capability: JSON alone does not prove that prepare ran or the caller saved it.

## Bounds and recovery

Schema 12 requires Store identity, the ledger and recovery barrier fields. The experimental
limit is 256 receipts, including cancellations; full stores reject new operations
without eviction. There is deliberately no release/expiry API until its durable
acknowledgement and nonce-reuse contract is designed. Cancellation before reservation directly records `cancelled_before_reservation`,
even after a competing state change or candidate collection; its
`reservation_generation` field denotes the terminal registration generation,
not an actual reservation. Only this terminal reason does not pin its candidate.
Current state and retained backups pin every other receipt candidate and every
committed directory. This favors safety
over reclamation and is not the final product retention policy.

State recovery unions compatible facts from the selected backup, current state
and retained backups. Reserved may advance to terminal; different requests or
conflicting terminal results reject recovery. Damaged/missing latest state sets
an enduring recovery barrier: known terminal facts can be queried, but missing or
reserved results cannot authorize publication or cancellation. Retention cannot
clear that barrier. There is no heuristic reconstruction from the current head.

An unpublished private generation can remain after a failed save; existing pins
and GC rules retain it conservatively. Doctor reports missing/mismatched committed
generation resources without changing historical receipts. Existing platform
flush/rename limits still apply; fault injection is not a power-loss simulator.

## Verification

Nine grouped receipt tests cover reserve/publish/cancel pre/post-rename failures,
failed acknowledgement barriers, actual concurrent retry, nonce rebinding,
capacity refusal, successor heads, explicit retention and real synthetic GC,
missing candidate payload, damaged-state recovery, terminal conflicts, required
schema fields, and missing generation diagnosis without resurrection.

Run `cargo test --locked --all-targets` and
`cargo clippy --locked --all-targets -- -D warnings`. Windows target type checking
is separate from Windows runtime verification; this stage performs no VSS test.

The Store identifier names the logical Store, not an authentication secret or a
physical machine. Copying its entire directory preserves that identity; it is
not a new independent Store clone. A future clone operation must assign a new
identity. Recovery never adopts a different identity from a backup.
