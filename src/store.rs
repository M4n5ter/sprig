//! One atomic state file publishes branch heads and workspace generations together.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::content::{self, EntryKind, Manifest, Observed};
use crate::copy_tree::{self, CopyMode};
use crate::merge::{self, MergeStrategy};
use crate::metadata;
use crate::model::{
    BranchResult, Change, CommitResult, DoctorReport, GcPlan, Listing, MergeOutcome, Mutation,
    Revision, RevisionSummary, State, Stats, Status, TrashItem, Workspace, WorkspaceResult,
};
use crate::{Blame, Result};

// Identifies the one supported layout; this is not a compatibility layer.
const STORAGE_SCHEMA: u32 = 3;
const AREAS: [&str; 5] = ["objects", "revisions", "native", "staging", "backups"];

/// Handle to a local, single-user store. Methods acquire their own metadata lock;
/// callers may clone handles, while branch-head checks prevent stale commits.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn init(path: &Path) -> Result<Self> {
        fs::DirBuilder::new().mode(0o700).create(path)?;
        let root = path.canonicalize()?;
        for area in AREAS {
            fs::create_dir(root.join(area))?;
            sync_directory(&root.join(area))?;
        }
        write_new(&root.join("lock"), b"")?;
        let state = State {
            version: STORAGE_SCHEMA,
            generation: 0,
            branches: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            trash: BTreeMap::new(),
        };
        write_new(&root.join("state.json"), &serde_json::to_vec(&state)?)?;
        sync_directory(&root)?;
        sync_directory(
            root.parent()
                .ok_or_else(|| failure("store has no parent"))?,
        )?;
        Self::open(&root)
    }

    pub fn open(path: &Path) -> Result<Self> {
        require_directory(path)?;
        let root = path.canonicalize()?;
        for area in AREAS {
            require_directory(&root.join(area))?;
        }
        require_file(&root.join("lock"))?;
        let store = Self { root };
        let _lock = store.lock()?;
        store.state()?;
        Ok(store)
    }

    /// Restore a previously saved metadata generation after validating every
    /// referenced revision and native tree. Current bytes are retained in staging.
    pub fn recover_state(path: &Path, generation: u64) -> Result<Status> {
        require_directory(path)?;
        let store = Self {
            root: path.canonicalize()?,
        };
        for area in AREAS {
            require_directory(&store.root.join(area))?;
        }
        {
            let _lock = store.lock()?;
            let backup = store
                .root
                .join("backups")
                .join(format!("{generation}.json.zst"));
            require_file(&backup)?;
            let (mut restored, _): (State, _) = metadata::read(&backup)?;
            validate_state(&restored)?;
            let roots = store.protected_roots(&restored)?;
            for id in store.ancestors(&roots)? {
                for hash in content::referenced_chunks(&store.revision(&id)?.manifest) {
                    let object = store.objects().join(&hash);
                    require_file(&object)?;
                    if blake3::hash(&fs::read(object)?).to_hex().as_str() != hash {
                        return Err(failure("corrupt content prevents metadata recovery"));
                    }
                }
            }
            let protected_directories = store.protected_directories(&restored)?;
            for directory in &protected_directories {
                require_directory(&store.root.join("native").join(directory))?;
                require_directory(&store.tree(directory))?;
            }
            // Active command leases must not be detached by metadata recovery.
            for directory in names(&store.root.join("native"))? {
                let native = store.root.join("native").join(&directory);
                require_directory(&native)?;
                let lease_path = native.join("lease");
                if !protected_directories.contains(&directory)
                    && fs::symlink_metadata(&lease_path)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                {
                    continue;
                }
                require_file(&lease_path)?;
                let lease = OpenOptions::new().read(true).write(true).open(lease_path)?;
                lease
                    .try_lock()
                    .map_err(|error| failure(&format!("workspace_busy: {error}")))?;
            }
            let current_path = store.root.join("state.json");
            require_file(&current_path)?;
            let current_bytes = fs::read(&current_path)?;
            let current_json = serde_json::from_slice::<serde_json::Value>(&current_bytes).ok();
            if let Some(version) = current_json.as_ref().and_then(|value| value.get("version"))
                && version != &serde_json::json!(STORAGE_SCHEMA)
            {
                return Err(failure("unsupported store schema; store was not modified"));
            }
            let current =
                current_json.and_then(|value| serde_json::from_value::<State>(value).ok());
            let highest = store
                .historical_states()?
                .iter()
                .map(|state| state.generation)
                .chain(current.iter().map(|state| state.generation))
                .max()
                .unwrap_or(generation);
            restored.generation = highest
                .checked_add(1)
                .ok_or_else(|| failure("metadata generation exhausted"))?;
            let retained = store
                .root
                .join("staging")
                .join(format!("retained-state-{}.json", new_id()?));
            write_new(&retained, &current_bytes)?;
            if let Some(current) = current.filter(|current| validate_state(current).is_ok()) {
                write_immutable(
                    &store
                        .root
                        .join("backups")
                        .join(format!("{}.json.zst", current.generation)),
                    &metadata::encode(&current_bytes)?,
                )?;
            }
            let staged = store
                .root
                .join("staging")
                .join(format!("recovered-state-{}.json", new_id()?));
            write_new(&staged, &serde_json::to_vec(&restored)?)?;
            sync_directory(&store.root.join("staging"))?;
            fs::rename(staged, current_path)?;
            sync_directory(&store.root)?;
        }
        store.status()
    }

    pub fn import(&self, source: &Path, name: &str, mode: CopyMode) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        require_directory(source)?;
        let source = source.canonicalize()?;
        if source.starts_with(&self.root) || self.root.starts_with(&source) {
            return Err(failure("source and store must not contain one another"));
        }
        let directory = self.new_native()?;
        let tree = self.tree(&directory);
        let copy_stats = copy_tree::copy_tree(&source, &tree, mode)?;
        let (manifest, observed) = content::capture(&tree, &self.objects(), &Observed::new())?;
        let revision = self.put_revision(&Revision {
            parents: vec![],
            clock: 1,
            message: format!("import {name}"),
            manifest,
        })?;
        let workspace = Workspace {
            branch: name.to_owned(),
            base_revision: revision.clone(),
            directory,
            observed,
            pending: None,
        };
        state.branches.insert(name.to_owned(), revision);
        state.workspaces.insert(name.to_owned(), workspace.clone());
        self.save(&mut state)?;
        let mut result = self.workspace_result(name, &workspace);
        result.copy_stats = Some(copy_stats);
        Ok(result)
    }

    pub fn fork(&self, source: &str, name: &str, mode: CopyMode) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let source = workspace(&state, source)?.clone();
        let _lease = self.lease(&source)?;
        // Capture identities first so a pending native rename stays identifiable.
        // The objects are deduplicated and remain unpublished until checkpoint.
        let (manifest, _) = content::capture(
            &self.tree(&source.directory),
            &self.objects(),
            &source.observed,
        )?;
        let directory = self.new_native()?;
        let copy_stats =
            copy_tree::copy_tree(&self.tree(&source.directory), &self.tree(&directory), mode)?;
        let observed = observe_manifest(&self.tree(&directory), &manifest)?;
        let fork = Workspace {
            branch: name.to_owned(),
            base_revision: source.base_revision.clone(),
            directory,
            observed,
            pending: source.pending.clone(),
        };
        state.branches.insert(name.to_owned(), source.base_revision);
        state.workspaces.insert(name.to_owned(), fork.clone());
        self.save(&mut state)?;
        let mut result = self.workspace_result(name, &fork);
        result.copy_stats = Some(copy_stats);
        Ok(result)
    }

    pub fn checkout(&self, branch: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, false)?;
        let revision = state
            .branches
            .get(branch)
            .ok_or_else(|| failure("branch not found"))?
            .clone();
        self.restore_locked(&mut state, &revision, branch, name, false)
    }

    pub fn restore(&self, reference: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let revision = self.resolve(&state, reference)?;
        self.restore_locked(&mut state, &revision, name, name, true)
    }

    fn restore_locked(
        &self,
        state: &mut State,
        revision: &str,
        branch: &str,
        name: &str,
        new_branch: bool,
    ) -> Result<WorkspaceResult> {
        let data = self.revision(revision)?;
        let directory = self.new_native()?;
        content::materialize(&data.manifest, &self.objects(), &self.tree(&directory))?;
        let observed = observe_manifest(&self.tree(&directory), &data.manifest)?;
        let workspace = Workspace {
            branch: branch.to_owned(),
            base_revision: revision.to_owned(),
            directory,
            observed,
            pending: None,
        };
        if new_branch {
            state
                .branches
                .insert(branch.to_owned(), revision.to_owned());
        }
        state.workspaces.insert(name.to_owned(), workspace.clone());
        self.save(state)?;
        Ok(self.workspace_result(name, &workspace))
    }

    pub fn checkpoint(
        &self,
        name: &str,
        message: &str,
        expected: Option<&str>,
    ) -> Result<CommitResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let workspace = workspace(&state, name)?.clone();
        let _lease = self.lease(&workspace)?;
        branch_head(&state, &workspace.branch, expected)?;
        let (manifest, observed) = content::capture(
            &self.tree(&workspace.directory),
            &self.objects(),
            &workspace.observed,
        )?;
        self.commit_workspace(&mut state, name, workspace, manifest, observed, message)
    }

    fn commit_workspace(
        &self,
        state: &mut State,
        name: &str,
        mut workspace: Workspace,
        manifest: Manifest,
        observed: Observed,
        message: &str,
    ) -> Result<CommitResult> {
        if workspace
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.conflicts.is_empty())
        {
            return Err(failure(
                "unresolved_conflicts: inspect show, edit the conflict generation, then explicitly resolve every path before checkpoint",
            ));
        }
        if workspace.pending.is_some() && tree_has_markers(&self.tree(&workspace.directory))? {
            return Err(failure(
                "unresolved_markers: conflict marker lines remain in the workspace",
            ));
        }
        let previous = branch_head(state, &workspace.branch, None)?;
        let parent = self.revision(&previous)?;
        let mut final_manifest = manifest;
        let mut final_observed = observed;
        let mut parents = vec![previous.clone()];
        let mut clock = parent.clock;
        if workspace.base_revision != previous {
            let base = self.revision(&workspace.base_revision)?;
            let candidate = self.put_revision(&Revision {
                parents: vec![workspace.base_revision.clone()],
                clock: base.clock + 1,
                message: message.to_owned(),
                manifest: final_manifest.clone(),
            })?;
            let merged = merge::merge(
                &base.manifest,
                &parent.manifest,
                &final_manifest,
                &self.objects(),
                MergeStrategy::Conflict,
                (parent.clock, &previous),
                (base.clock + 1, &candidate),
                &[],
            )?;
            let directory = self.new_native()?;
            content::materialize(&merged.manifest, &self.objects(), &self.tree(&directory))?;
            let observed = observe_manifest(&self.tree(&directory), &merged.manifest)?;
            let token = new_id()?;
            state.trash.insert(
                token.clone(),
                TrashItem {
                    token,
                    name: format!("pre-merge-{name}"),
                    revision: workspace.base_revision.clone(),
                    workspace: Some(workspace.clone()),
                },
            );
            workspace.directory = directory;
            workspace.base_revision = previous.clone();
            workspace.observed = observed.clone();
            if !merged.conflicts.is_empty() {
                self.write_conflict_markers(
                    &self.tree(&workspace.directory),
                    &merged.manifest,
                    &merged.conflicts,
                )?;
                workspace.pending = Some(crate::PendingMerge {
                    candidate_revision: candidate,
                    conflicts: merged.conflicts,
                });
                sync_native_tree(&self.tree(&workspace.directory))?;
                state.workspaces.insert(name.to_owned(), workspace);
                self.save(state)?;
                return Err(failure(
                    "merge_conflict: branch unchanged; a recoverable conflict generation is now active. Use show to inspect conflicts, edit, resolve, then checkpoint",
                ));
            }
            final_manifest = merged.manifest;
            final_observed = observed;
            parents.push(candidate);
            clock = clock.max(base.clock + 1);
        } else if let Some(pending) = &workspace.pending {
            parents.push(pending.candidate_revision.clone());
            clock = clock.max(self.revision(&pending.candidate_revision)?.clock);
        }
        let revision = if parent.manifest == final_manifest {
            previous.clone()
        } else {
            self.put_revision(&Revision {
                parents,
                clock: clock
                    .checked_add(1)
                    .ok_or_else(|| failure("logical clock exhausted"))?,
                message: message.to_owned(),
                manifest: final_manifest,
            })?
        };
        workspace.base_revision = revision.clone();
        workspace.observed = final_observed;
        workspace.pending = None;
        let branch = workspace.branch.clone();
        state.branches.insert(branch.clone(), revision.clone());
        sync_native_tree(&self.tree(&workspace.directory))?;
        state.workspaces.insert(name.to_owned(), workspace);
        self.save(state)?;
        Ok(CommitResult {
            committed: true,
            revision,
            branch,
            previous_head: Some(previous),
        })
    }

    fn write_conflict_markers(
        &self,
        tree: &Path,
        manifest: &Manifest,
        conflicts: &[merge::MergeConflict],
    ) -> Result<()> {
        for conflict in conflicts {
            let (Some(base), Some(ours), Some(theirs)) =
                (&conflict.base, &conflict.ours, &conflict.theirs)
            else {
                continue;
            };
            // Never trust an old pathname after identity-aware rename merging.
            // A new unrelated entry may now occupy that pathname.
            let Some(entry) = manifest.get(&conflict.path) else {
                continue;
            };
            if entry.id != ours.id || !matches!(entry.kind, EntryKind::File { .. }) {
                continue;
            }
            if !matches!(
                (&base.kind, &ours.kind, &theirs.kind),
                (
                    EntryKind::File { .. },
                    EntryKind::File { .. },
                    EntryKind::File { .. }
                )
            ) {
                continue;
            }
            let (Ok(base), Ok(ours), Ok(theirs)) = (
                content::read_file(base, &self.objects()),
                content::read_file(ours, &self.objects()),
                content::read_file(theirs, &self.objects()),
            ) else {
                continue;
            };
            let (Ok(base), Ok(ours), Ok(theirs)) = (
                std::str::from_utf8(&base),
                std::str::from_utf8(&ours),
                std::str::from_utf8(&theirs),
            ) else {
                continue;
            };
            if base.contains('\0') || ours.contains('\0') || theirs.contains('\0') {
                continue;
            }
            if let Err(markers) = diffy::merge(base, ours, theirs) {
                let Ok(target) = mutation_path(tree, &conflict.path, false) else {
                    continue;
                };
                if require_file(&target).is_err() {
                    continue;
                }
                fs::write(&target, markers.as_bytes())?;
                File::open(target)?.sync_all()?;
            }
        }
        Ok(())
    }

    pub fn resolve_conflicts(&self, name: &str, paths: &[String]) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let mut workspace = workspace(&state, name)?.clone();
        let _lease = self.lease(&workspace)?;
        let Some(pending) = &mut workspace.pending else {
            return Err(failure("workspace has no pending merge"));
        };
        if paths.is_empty() {
            return Err(failure("explicit conflict paths are required"));
        }
        for path in paths {
            if !pending
                .conflicts
                .iter()
                .any(|conflict| conflict.path == *path)
            {
                return Err(failure("path is not an unresolved conflict"));
            }
            let target = if path.is_empty() {
                self.tree(&workspace.directory)
            } else {
                resolution_path(&self.tree(&workspace.directory), path)?
            };
            if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_file())
                && file_has_markers(&target)?
            {
                return Err(failure(
                    "unresolved_markers: remove marker lines before resolving",
                ));
            }
        }
        pending
            .conflicts
            .retain(|conflict| !paths.contains(&conflict.path));
        state.workspaces.insert(name.to_owned(), workspace.clone());
        self.save(&mut state)?;
        Ok(self.workspace_result(name, &workspace))
    }

    pub fn branch(&self, name: &str, reference: &str) -> Result<BranchResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        validate_name(name)?;
        if state.branches.contains_key(name) {
            return Err(failure("branch already exists"));
        }
        let revision = self.resolve(&state, reference)?;
        state.branches.insert(name.to_owned(), revision.clone());
        self.save(&mut state)?;
        Ok(BranchResult {
            name: name.to_owned(),
            revision,
        })
    }

    pub fn merge(
        &self,
        branch: &str,
        theirs: &str,
        output: &str,
        strategy: MergeStrategy,
        units: &[Vec<String>],
        expected: Option<&str>,
    ) -> Result<MergeOutcome> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        validate_name(output)?;
        if state.branches.contains_key(output) {
            return Err(failure("merge output branch already exists"));
        }
        let ours_id = branch_head(&state, branch, expected)?;
        let theirs_id = self.resolve(&state, theirs)?;
        let base_id = self.merge_base(&ours_id, &theirs_id)?;
        if base_id == ours_id || base_id == theirs_id {
            let revision = if base_id == ours_id {
                theirs_id
            } else {
                ours_id
            };
            state.branches.insert(output.to_owned(), revision.clone());
            self.save(&mut state)?;
            return Ok(MergeOutcome {
                committed: true,
                revision: Some(revision),
                branch: output.to_owned(),
                merge_base: base_id,
                conflicts: vec![],
            });
        }
        let base = self.revision(&base_id)?;
        let ours = self.revision(&ours_id)?;
        let theirs = self.revision(&theirs_id)?;
        let merged = merge::merge(
            &base.manifest,
            &ours.manifest,
            &theirs.manifest,
            &self.objects(),
            strategy,
            (ours.clock, &ours_id),
            (theirs.clock, &theirs_id),
            units,
        )?;
        if !merged.conflicts.is_empty() {
            return Ok(MergeOutcome {
                committed: false,
                revision: None,
                branch: output.to_owned(),
                merge_base: base_id,
                conflicts: merged.conflicts,
            });
        }
        let revision = self.put_revision(&Revision {
            parents: vec![ours_id, theirs_id],
            clock: ours
                .clock
                .max(theirs.clock)
                .checked_add(1)
                .ok_or_else(|| failure("logical clock exhausted"))?,
            message: format!("merge {branch} into {output}"),
            manifest: merged.manifest,
        })?;
        state.branches.insert(output.to_owned(), revision.clone());
        self.save(&mut state)?;
        Ok(MergeOutcome {
            committed: true,
            revision: Some(revision),
            branch: output.to_owned(),
            merge_base: base_id,
            conflicts: vec![],
        })
    }

    pub fn rollback(
        &self,
        branch: &str,
        reference: &str,
        message: &str,
        expected: Option<&str>,
    ) -> Result<CommitResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let old = branch_head(&state, branch, expected)?;
        let target = self.resolve(&state, reference)?;
        let old_revision = self.revision(&old)?;
        let revision = self.put_revision(&Revision {
            parents: vec![old.clone()],
            clock: old_revision
                .clock
                .checked_add(1)
                .ok_or_else(|| failure("logical clock exhausted"))?,
            message: message.to_owned(),
            manifest: self.revision(&target)?.manifest,
        })?;
        state.branches.insert(branch.to_owned(), revision.clone());
        self.save(&mut state)?;
        Ok(CommitResult {
            committed: true,
            revision,
            branch: branch.to_owned(),
            previous_head: Some(old),
        })
    }

    /// Transactional SDK mutations work in a new native generation. Failure never
    /// publishes a partial tree or destroys the caller's current working tree.
    pub fn apply(
        &self,
        name: &str,
        mutations: &[Mutation],
        message: &str,
        expected: Option<&str>,
    ) -> Result<CommitResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let original = workspace(&state, name)?.clone();
        let _lease = self.lease(&original)?;
        branch_head(&state, &original.branch, expected)?;
        let (original_manifest, _) = content::capture(
            &self.tree(&original.directory),
            &self.objects(),
            &original.observed,
        )?;
        let directory = self.new_native()?;
        let tree = self.tree(&directory);
        copy_tree::copy_tree(&self.tree(&original.directory), &tree, CopyMode::Auto)?;
        let mut observed = observe_manifest(&tree, &original_manifest)?;
        for mutation in mutations {
            apply_mutation(&tree, &mut observed, mutation)?;
        }
        let (manifest, observed) = content::capture(&tree, &self.objects(), &observed)?;
        let mut replacement = original.clone();
        replacement.directory = directory;
        // Retain the entire old working generation, including uncommitted data.
        let token = new_id()?;
        state.trash.insert(
            token.clone(),
            TrashItem {
                token,
                name: format!("generation-{name}"),
                revision: original.base_revision.clone(),
                workspace: Some(original),
            },
        );
        if replacement.pending.is_some() {
            replacement.observed = observed;
            let result = CommitResult {
                committed: false,
                revision: replacement.base_revision.clone(),
                branch: replacement.branch.clone(),
                previous_head: Some(branch_head(&state, &replacement.branch, expected)?),
            };
            sync_native_tree(&self.tree(&replacement.directory))?;
            state.workspaces.insert(name.to_owned(), replacement);
            self.save(&mut state)?;
            return Ok(result);
        }
        self.commit_workspace(&mut state, name, replacement, manifest, observed, message)
    }

    pub fn exec(&self, name: &str, program: &str, arguments: &[String]) -> Result<i32> {
        let (workspace, lease) = {
            let _lock = self.lock()?;
            let state = self.state()?;
            let workspace = workspace(&state, name)?.clone();
            let lease = self.lease(&workspace)?;
            (workspace, lease)
        };
        // This is a coordination lease, NOT a security sandbox. Keep the file
        // alive while waiting, but release the metadata lock for other workspaces.
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let descriptor = lease.as_raw_fd();
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(self.tree(&workspace.directory));
        // SAFETY: the child hook performs only async-signal-safe fcntl calls.
        // The live lease fd is inherited across exec so killing this supervising
        // process cannot silently unlock a child that is still writing.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(descriptor, libc::F_GETFD);
                if flags == -1
                    || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let status = command.status()?;
        drop(lease);
        Ok(status.code().unwrap_or(128))
    }

    pub fn drop_entry(&self, kind: &str, name: &str) -> Result<TrashItem> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        validate_name(name)?;
        let token = new_id()?;
        let item = match kind {
            "workspace" => {
                let workspace = workspace(&state, name)?.clone();
                let _lease = self.lease(&workspace)?;
                state.workspaces.remove(name);
                TrashItem {
                    token: token.clone(),
                    name: name.to_owned(),
                    revision: workspace.base_revision.clone(),
                    workspace: Some(workspace),
                }
            }
            "branch" => {
                if state
                    .workspaces
                    .values()
                    .any(|workspace| workspace.branch == name)
                {
                    return Err(failure(
                        "branch is attached to a workspace; drop its workspaces first",
                    ));
                }
                let revision = state
                    .branches
                    .remove(name)
                    .ok_or_else(|| failure("branch not found"))?;
                TrashItem {
                    token: token.clone(),
                    name: name.to_owned(),
                    revision,
                    workspace: None,
                }
            }
            _ => return Err(failure("drop kind must be workspace or branch")),
        };
        state.trash.insert(token, item.clone());
        self.save(&mut state)?;
        Ok(item)
    }

    pub fn recover(&self, token: &str, name: &str, mode: CopyMode) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let item = state
            .trash
            .get(token)
            .ok_or_else(|| failure("trash token not found"))?
            .clone();
        match item.workspace {
            None => self.restore_locked(&mut state, &item.revision, name, name, true),
            Some(old) => {
                let _lease = self.lease(&old)?;
                let (manifest, _) =
                    content::capture(&self.tree(&old.directory), &self.objects(), &old.observed)?;
                let directory = self.new_native()?;
                let copy_stats =
                    copy_tree::copy_tree(&self.tree(&old.directory), &self.tree(&directory), mode)?;
                let observed = observe_manifest(&self.tree(&directory), &manifest)?;
                let workspace = Workspace {
                    branch: name.to_owned(),
                    base_revision: old.base_revision.clone(),
                    directory,
                    observed,
                    pending: old.pending.clone(),
                };
                state.branches.insert(name.to_owned(), old.base_revision);
                state.workspaces.insert(name.to_owned(), workspace.clone());
                self.save(&mut state)?;
                let mut result = self.workspace_result(name, &workspace);
                result.copy_stats = Some(copy_stats);
                Ok(result)
            }
        }
    }

    pub fn show(&self, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let state = self.state()?;
        Ok(self.workspace_result(name, workspace(&state, name)?))
    }

    pub fn list(&self) -> Result<Listing> {
        let _lock = self.lock()?;
        Ok(self.listing(&self.state()?))
    }

    pub fn status(&self) -> Result<Status> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let stale_workspaces = state
            .workspaces
            .iter()
            .filter(|(_, workspace)| {
                state.branches.get(&workspace.branch) != Some(&workspace.base_revision)
            })
            .map(|(name, _)| name.clone())
            .collect();
        let mut active_leases = Vec::new();
        for (name, workspace) in &state.workspaces {
            if self.lease(workspace).is_err() {
                active_leases.push(name.clone());
            }
        }
        Ok(Status {
            generation: state.generation,
            store: self.root.clone(),
            listing: self.listing(&state),
            stale_workspaces,
            active_leases,
        })
    }

    pub fn log(&self, reference: Option<&str>) -> Result<Vec<RevisionSummary>> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let roots = match reference {
            Some(reference) => vec![self.resolve(&state, reference)?],
            None => state.branches.values().cloned().collect(),
        };
        let mut result = Vec::new();
        for id in self.ancestors(&roots)? {
            let revision = self.revision(&id)?;
            result.push(RevisionSummary {
                revision: id,
                parents: revision.parents,
                clock: revision.clock,
                message: revision.message,
            });
        }
        result.sort_by(|left, right| {
            right
                .clock
                .cmp(&left.clock)
                .then_with(|| left.revision.cmp(&right.revision))
        });
        Ok(result)
    }

    pub fn diff(&self, from: &str, to: &str) -> Result<Vec<Change>> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let before = self.revision(&self.resolve(&state, from)?)?.manifest;
        let after = self.revision(&self.resolve(&state, to)?)?.manifest;
        let by_id: BTreeMap<_, _> = before
            .iter()
            .map(|(path, entry)| (entry.id.as_str(), (path, entry)))
            .collect();
        let after_ids: BTreeSet<_> = after.values().map(|entry| entry.id.as_str()).collect();
        let mut changes = Vec::new();
        for (path, entry) in &after {
            match by_id.get(entry.id.as_str()) {
                None => changes.push(Change {
                    path: path.clone(),
                    previous_path: None,
                    kind: "added".to_owned(),
                }),
                Some((previous, old)) if *previous != path || *old != entry => {
                    changes.push(Change {
                        path: path.clone(),
                        previous_path: if *previous != path {
                            Some((*previous).clone())
                        } else {
                            None
                        },
                        kind: if *previous != path {
                            "renamed_or_moved"
                        } else {
                            "modified"
                        }
                        .to_owned(),
                    })
                }
                _ => {}
            }
        }
        for (path, entry) in &before {
            if !after_ids.contains(entry.id.as_str()) {
                changes.push(Change {
                    path: path.clone(),
                    previous_path: None,
                    kind: "removed".to_owned(),
                });
            }
        }
        Ok(changes)
    }

    pub fn diff_files(&self, from: &str, to: &str) -> Result<Vec<crate::FileDiff>> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let before = self.revision(&self.resolve(&state, from)?)?.manifest;
        let after = self.revision(&self.resolve(&state, to)?)?.manifest;
        let before_ids: BTreeMap<_, _> = before
            .iter()
            .map(|(path, entry)| (entry.id.as_str(), (path, entry)))
            .collect();
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        for (path, entry) in &after {
            let old = before_ids.get(entry.id.as_str()).copied();
            if let Some((old_path, old_entry)) = old {
                seen.insert(old_path.clone());
                if *old_path == *path && old_entry == entry {
                    continue;
                }
            }
            result.push(self.file_diff(path, old, Some(entry))?);
        }
        for (path, entry) in &before {
            if !seen.contains(path) && !after.values().any(|new| new.id == entry.id) {
                result.push(self.file_diff(path, Some((path, entry)), None)?);
            }
        }
        Ok(result)
    }

    fn file_diff(
        &self,
        path: &str,
        old: Option<(&String, &content::Entry)>,
        new: Option<&content::Entry>,
    ) -> Result<crate::FileDiff> {
        let load = |entry: Option<&content::Entry>| -> Result<Option<String>> {
            match entry {
                None => Ok(Some(String::new())),
                Some(entry) if matches!(entry.kind, EntryKind::File { size, .. } if size <= content::MAX_READ_FILE) =>
                {
                    let bytes = content::read_file(entry, &self.objects())?;
                    Ok(String::from_utf8(bytes)
                        .ok()
                        .filter(|text| !text.contains('\0')))
                }
                _ => Ok(None),
            }
        };
        let previous_path = old
            .filter(|(old_path, _)| old_path.as_str() != path)
            .map(|(path, _)| path.clone());
        let before = load(old.map(|(_, entry)| entry))?;
        let after = load(new)?;
        let patch = match (before, after) {
            (Some(before), Some(after)) => Some(diffy::create_patch(&before, &after).to_string()),
            _ => None,
        };
        Ok(crate::FileDiff {
            path: path.to_owned(),
            previous_path,
            kind: if old.is_none() {
                "added"
            } else if new.is_none() {
                "removed"
            } else {
                "modified"
            }
            .to_owned(),
            binary: patch.is_none(),
            patch,
        })
    }

    /// Attribute text lines along first-parent history, following logical identity.
    /// Merge-introduced lines are attributed to that merge, not its second parent.
    pub fn blame(&self, reference: &str, path: &str) -> Result<Blame> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let revision = self.resolve(&state, reference)?;
        crate::blame::attribute(&revision, path, &self.objects(), |id| self.revision(id))
    }

    pub fn doctor(&self) -> Result<DoctorReport> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let mut errors = Vec::new();
        let roots = self.protected_roots(&state)?;
        let reachable = match self.ancestors(&roots) {
            Ok(ids) => ids,
            Err(error) => {
                errors.push(error.to_string());
                BTreeSet::new()
            }
        };
        let mut referenced = BTreeSet::new();
        for id in &reachable {
            match self.revision(id) {
                Ok(revision) => {
                    referenced.extend(content::referenced_chunks(&revision.manifest));
                }
                Err(error) => errors.push(format!("revision {id}: {error}")),
            }
        }
        for hash in referenced {
            let path = self.objects().join(&hash);
            let check = (|| -> Result<()> {
                require_file(&path)?;
                if blake3::hash(&fs::read(&path)?).to_hex().as_str() != hash {
                    return Err(failure("content hash mismatch"));
                }
                Ok(())
            })();
            if let Err(error) = check {
                errors.push(format!("object {hash}: {error}"));
            }
        }
        for (name, workspace) in &state.workspaces {
            if let Err(error) = require_directory(&self.tree(&workspace.directory)) {
                errors.push(format!("workspace {name}: {error}"));
            }
        }
        let protected_directories = self.protected_directories(&state)?;
        let orphan_workspaces = names(&self.root.join("native"))?
            .into_iter()
            .filter(|name| !protected_directories.contains(name))
            .collect();
        let orphan_revisions = names(&self.root.join("revisions"))?
            .into_iter()
            .filter_map(|name| name.strip_suffix(".json.zst").map(str::to_owned))
            .filter(|id| !reachable.contains(id))
            .collect();
        Ok(DoctorReport {
            healthy: errors.is_empty(),
            errors,
            orphan_revisions,
            orphan_workspaces,
            staging_entries: names(&self.root.join("staging"))?,
        })
    }

    /// A conservative dry-run only. State backups are roots, so neither dropped
    /// work nor previously published history silently becomes collectible.
    pub fn gc(&self) -> Result<GcPlan> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let reachable = self.ancestors(&self.protected_roots(&state)?)?;
        let mut objects = BTreeSet::new();
        for id in &reachable {
            objects.extend(content::referenced_chunks(&self.revision(id)?.manifest));
        }
        let unreachable_objects: Vec<_> = names(&self.objects())?
            .into_iter()
            .filter(|name| valid_hash(name) && !objects.contains(name))
            .collect();
        let unreachable_revisions = names(&self.root.join("revisions"))?
            .into_iter()
            .filter_map(|name| name.strip_suffix(".json.zst").map(str::to_owned))
            .filter(|id| !reachable.contains(id))
            .collect();
        let directories = self.protected_directories(&state)?;
        let orphan_workspaces = names(&self.root.join("native"))?
            .into_iter()
            .filter(|name| !directories.contains(name))
            .collect();
        let mut reclaimable_bytes = 0;
        for hash in &unreachable_objects {
            reclaimable_bytes += fs::symlink_metadata(self.objects().join(hash))?.len();
        }
        Ok(GcPlan { dry_run: true, generation: state.generation, unreachable_objects, unreachable_revisions, orphan_workspaces, reclaimable_bytes, note: "No files deleted. All heads, workspace bases, trash, parents and state backups are protected. No permanent purge API is provided.".to_owned() })
    }

    pub fn stats(&self) -> Result<Stats> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let revisions = self.ancestors(&self.protected_roots(&state)?)?;
        let objects: Vec<_> = names(&self.objects())?
            .into_iter()
            .filter(|name| valid_hash(name))
            .collect();
        let mut object_bytes = 0;
        for hash in &objects {
            object_bytes += fs::symlink_metadata(self.objects().join(hash))?.len();
        }
        let mut reachable_logical_bytes = 0;
        for id in &revisions {
            for entry in self.revision(id)?.manifest.values() {
                if let EntryKind::File { size, .. } = entry.kind {
                    reachable_logical_bytes += size;
                }
            }
        }
        Ok(Stats {
            revisions: revisions.len(),
            objects: objects.len(),
            object_bytes,
            reachable_logical_bytes,
            workspaces: state.workspaces.len(),
            branches: state.branches.len(),
            trash_entries: state.trash.len(),
        })
    }

    /// Read a committed regular file. References resolve once under the store lock.
    /// Use read_to for streaming large files rather than allocating a Vec.
    pub fn read(&self, reference: &str, path: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.read_to(reference, path, &mut bytes)?;
        Ok(bytes)
    }

    pub fn read_to(&self, reference: &str, path: &str, output: &mut impl Write) -> Result<()> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let revision = self.revision(&self.resolve(&state, reference)?)?;
        let entry = revision
            .manifest
            .get(path)
            .ok_or_else(|| failure("path not found"))?;
        let EntryKind::File { size, chunks } = &entry.kind else {
            return Err(failure(
                "read requires a regular file; symbolic links are not followed",
            ));
        };
        let mut written = 0u64;
        for hash in chunks {
            let object = self.objects().join(hash);
            require_file(&object)?;
            let bytes = fs::read(object)?;
            if blake3::hash(&bytes).to_hex().as_str() != hash {
                return Err(failure("content hash mismatch"));
            }
            written = written
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| failure("file size overflow"))?;
            if written > *size {
                return Err(failure("manifest file size mismatch"));
            }
            output.write_all(&bytes)?;
        }
        if written != *size {
            return Err(failure("manifest file size mismatch"));
        }
        Ok(())
    }

    pub fn ls(&self, reference: &str, path: &str) -> Result<Vec<crate::PathEntry>> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let manifest = self.revision(&self.resolve(&state, reference)?)?.manifest;
        if !matches!(
            manifest.get(path).map(|entry| &entry.kind),
            Some(EntryKind::Directory)
        ) {
            return Err(failure("ls requires a directory"));
        }
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        Ok(manifest
            .into_iter()
            .filter(|(key, _)| {
                key != path
                    && key
                        .strip_prefix(&prefix)
                        .is_some_and(|tail| !tail.contains('/'))
            })
            .map(|(path, entry)| crate::PathEntry { path, entry })
            .collect())
    }

    pub fn stat(&self, reference: &str, path: &str) -> Result<content::Entry> {
        let _lock = self.lock()?;
        let state = self.state()?;
        self.revision(&self.resolve(&state, reference)?)?
            .manifest
            .get(path)
            .cloned()
            .ok_or_else(|| failure("path not found"))
    }

    pub fn glob(&self, reference: &str, pattern: &str) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let matcher = globset::Glob::new(pattern)?.compile_matcher();
        Ok(self
            .revision(&self.resolve(&state, reference)?)?
            .manifest
            .into_keys()
            .filter(|path| matcher.is_match(path))
            .collect())
    }

    /// Literal, UTF-8 text search in files up to the documented merge read bound.
    pub fn grep(&self, reference: &str, pattern: &str) -> Result<Vec<crate::GrepMatch>> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let manifest = self.revision(&self.resolve(&state, reference)?)?.manifest;
        let mut matches = Vec::new();
        for (path, entry) in manifest {
            if matches!(entry.kind, EntryKind::File { size, .. } if size <= content::MAX_READ_FILE)
            {
                let bytes = content::read_file(&entry, &self.objects())?;
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    if text.contains('\0') {
                        continue;
                    }
                    for (number, line) in text.lines().enumerate() {
                        if line.contains(pattern) {
                            matches.push(crate::GrepMatch {
                                path: path.clone(),
                                line: number + 1,
                                text: line.to_owned(),
                            });
                        }
                    }
                }
            }
        }
        Ok(matches)
    }

    pub fn backend(&self) -> Result<copy_tree::NativeBackendInfo> {
        Ok(copy_tree::native_backend_info(&self.root)?)
    }

    fn listing(&self, state: &State) -> Listing {
        Listing {
            branches: state.branches.clone(),
            workspaces: state
                .workspaces
                .iter()
                .map(|(name, workspace)| (name.clone(), self.workspace_result(name, workspace)))
                .collect(),
            trash: state.trash.values().cloned().collect(),
        }
    }

    fn workspace_result(&self, name: &str, workspace: &Workspace) -> WorkspaceResult {
        WorkspaceResult {
            name: name.to_owned(),
            branch: workspace.branch.clone(),
            base_revision: workspace.base_revision.clone(),
            path: self.tree(&workspace.directory),
            pending_conflicts: workspace
                .pending
                .as_ref()
                .map(|pending| pending.conflicts.clone())
                .unwrap_or_default(),
            copy_stats: None,
        }
    }

    fn lock(&self) -> Result<File> {
        let path = self.root.join("lock");
        require_file(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock()
            .map_err(|error| failure(&format!("store_locked: {error}")))?;
        Ok(file)
    }

    fn lease(&self, workspace: &Workspace) -> Result<File> {
        let directory = self.root.join("native").join(&workspace.directory);
        require_directory(&directory)?;
        require_directory(&directory.join("tree"))?;
        let path = directory.join("lease");
        require_file(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(|error| {
            failure(&format!(
                "workspace_busy: active exec lease or lock error: {error}"
            ))
        })?;
        Ok(file)
    }

    fn state(&self) -> Result<State> {
        require_file(&self.root.join("state.json"))?;
        let state: State = serde_json::from_slice(&fs::read(self.root.join("state.json"))?)?;
        validate_state(&state)?;
        Ok(state)
    }

    fn save(&self, state: &mut State) -> Result<()> {
        let current = self.state()?;
        if current.generation != state.generation {
            return Err(failure(
                "metadata generation changed; transaction not published",
            ));
        }
        let backup = self
            .root
            .join("backups")
            .join(format!("{}.json.zst", current.generation));
        write_immutable(&backup, &metadata::encode(&serde_json::to_vec(&current)?)?)?;
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| failure("metadata generation exhausted"))?;
        validate_state(state)?;
        for workspace in state.workspaces.values() {
            require_directory(&self.tree(&workspace.directory))?;
            sync_directory(&self.root.join("native").join(&workspace.directory))?;
        }
        let staged = self
            .root
            .join("staging")
            .join(format!("state-{}.json", new_id()?));
        write_new(&staged, &serde_json::to_vec(state)?)?;
        fs::rename(staged, self.root.join("state.json"))?;
        // An error after rename has an uncertain publication outcome. Inspect
        // generation/head before retrying; never blindly repeat a mutation.
        sync_directory(&self.root)?;
        sync_directory(&self.root.join("staging"))?;
        Ok(())
    }

    fn put_revision(&self, revision: &Revision) -> Result<String> {
        content::validate_manifest(&revision.manifest)?;
        for parent in &revision.parents {
            if !valid_hash(parent) {
                return Err(failure("invalid revision parent"));
            }
        }
        let bytes = serde_json::to_vec(revision)?;
        let id = blake3::hash(&bytes).to_hex().to_string();
        write_immutable(
            &self.root.join("revisions").join(format!("{id}.json.zst")),
            &metadata::encode(&bytes)?,
        )?;
        Ok(id)
    }

    fn revision(&self, id: &str) -> Result<Revision> {
        if !valid_hash(id) {
            return Err(failure("invalid revision hash"));
        }
        let path = self.root.join("revisions").join(format!("{id}.json.zst"));
        require_file(&path)?;
        let (revision, hash): (Revision, _) = metadata::read(&path)?;
        if hash.to_hex().as_str() != id {
            return Err(failure("revision hash mismatch"));
        }
        if revision.clock == 0 || revision.parents.iter().any(|parent| !valid_hash(parent)) {
            return Err(failure("invalid revision metadata"));
        }
        content::validate_manifest(&revision.manifest)?;
        Ok(revision)
    }

    fn resolve(&self, state: &State, reference: &str) -> Result<String> {
        let id = if let Some(name) = reference.strip_prefix("workspace:") {
            workspace(state, name)?.base_revision.clone()
        } else if let Some(id) = state.branches.get(reference) {
            id.clone()
        } else if valid_hash(reference) {
            reference.to_owned()
        } else {
            return Err(failure(
                "unknown branch/revision; workspace:NAME resolves its committed base",
            ));
        };
        self.revision(&id)?;
        Ok(id)
    }

    fn new_native(&self) -> Result<String> {
        let id = new_id()?;
        let path = self.root.join("native").join(&id);
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        write_new(&path.join("lease"), b"")?;
        sync_directory(&path)?;
        sync_directory(&self.root.join("native"))?;
        Ok(id)
    }

    fn tree(&self, directory: &str) -> PathBuf {
        self.root.join("native").join(directory).join("tree")
    }
    fn objects(&self) -> PathBuf {
        self.root.join("objects")
    }

    fn ancestors(&self, roots: &[String]) -> Result<BTreeSet<String>> {
        let mut queue: VecDeque<_> = roots.iter().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(id) = queue.pop_front() {
            if seen.insert(id.clone()) {
                queue.extend(self.revision(&id)?.parents);
            }
        }
        Ok(seen)
    }

    fn merge_base(&self, ours: &str, theirs: &str) -> Result<String> {
        let left = self.ancestors(&[ours.to_owned()])?;
        let right = self.ancestors(&[theirs.to_owned()])?;
        let mut candidates = Vec::new();
        for id in left.intersection(&right) {
            candidates.push((self.revision(id)?.clock, id.clone()));
        }
        // A highest logical clock common ancestor is deterministic and cannot be
        // an ancestor of another common candidate with a strictly larger clock.
        candidates.sort();
        candidates
            .pop()
            .map(|(_, id)| id)
            .ok_or_else(|| failure("branches have no common ancestor"))
    }

    fn historical_states(&self) -> Result<Vec<State>> {
        let mut states = Vec::new();
        for name in names(&self.root.join("backups"))? {
            if !name.ends_with(".json.zst") {
                return Err(failure(
                    "unknown state backup file; refuse unsafe reachability plan",
                ));
            }
            let path = self.root.join("backups").join(name);
            require_file(&path)?;
            let (state, _): (State, _) = metadata::read(&path)?;
            validate_state(&state)?;
            states.push(state);
        }
        Ok(states)
    }

    fn protected_roots(&self, state: &State) -> Result<Vec<String>> {
        let mut states = self.historical_states()?;
        states.push(state.clone());
        let mut roots = BTreeSet::new();
        for state in states {
            roots.extend(state.branches.into_values());
            for workspace in state.workspaces.into_values() {
                roots.insert(workspace.base_revision);
                if let Some(pending) = workspace.pending {
                    roots.insert(pending.candidate_revision);
                }
            }
            for item in state.trash.into_values() {
                roots.insert(item.revision);
                if let Some(workspace) = item.workspace {
                    roots.insert(workspace.base_revision);
                    if let Some(pending) = workspace.pending {
                        roots.insert(pending.candidate_revision);
                    }
                }
            }
        }
        Ok(roots.into_iter().collect())
    }

    fn protected_directories(&self, state: &State) -> Result<BTreeSet<String>> {
        let mut states = self.historical_states()?;
        states.push(state.clone());
        let mut directories = BTreeSet::new();
        for state in states {
            directories.extend(
                state
                    .workspaces
                    .into_values()
                    .map(|workspace| workspace.directory),
            );
            directories.extend(
                state
                    .trash
                    .into_values()
                    .filter_map(|item| item.workspace.map(|workspace| workspace.directory)),
            );
        }
        Ok(directories)
    }
}

fn branch_head(state: &State, branch: &str, expected: Option<&str>) -> Result<String> {
    let head = state
        .branches
        .get(branch)
        .ok_or_else(|| failure("branch not found"))?;
    if let Some(expected) = expected
        && expected != head
    {
        return Err(failure(
            "head_conflict: expected branch head does not match",
        ));
    }
    Ok(head.clone())
}

fn workspace<'a>(state: &'a State, name: &str) -> Result<&'a Workspace> {
    validate_name(name)?;
    state
        .workspaces
        .get(name)
        .ok_or_else(|| failure("workspace not found"))
}

fn fresh_name(state: &State, name: &str, branch: bool) -> Result<()> {
    validate_name(name)?;
    if state.workspaces.contains_key(name) || (branch && state.branches.contains_key(name)) {
        return Err(failure(
            "name already exists; no existing entry is overwritten",
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(failure(
            "names must be 1–96 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_state(state: &State) -> Result<()> {
    if state.version != STORAGE_SCHEMA {
        return Err(failure("unsupported store schema; store was not modified"));
    }
    for (name, revision) in &state.branches {
        validate_name(name)?;
        if !valid_hash(revision) {
            return Err(failure("invalid branch revision"));
        }
    }
    for (name, workspace) in &state.workspaces {
        validate_name(name)?;
        validate_workspace(workspace)?;
        if !state.branches.contains_key(&workspace.branch) {
            return Err(failure("workspace refers to a missing branch"));
        }
    }
    for (token, item) in &state.trash {
        validate_name(token)?;
        if item.token != *token || !valid_hash(&item.revision) {
            return Err(failure("invalid trash metadata"));
        }
        if let Some(workspace) = &item.workspace {
            validate_workspace(workspace)?;
        }
    }
    Ok(())
}

fn validate_workspace(workspace: &Workspace) -> Result<()> {
    validate_name(&workspace.branch)?;
    validate_name(&workspace.directory)?;
    if !valid_hash(&workspace.base_revision) {
        return Err(failure("invalid workspace base"));
    }
    if workspace
        .pending
        .as_ref()
        .is_some_and(|pending| !valid_hash(&pending.candidate_revision))
    {
        return Err(failure("invalid pending merge"));
    }
    Ok(())
}

fn new_id() -> Result<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!(
        "{nanos}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn require_directory(path: &Path) -> Result<()> {
    let path: PathBuf = path.components().collect();
    if !fs::symlink_metadata(&path)?.is_dir() {
        return Err(failure(&format!(
            "not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_file(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(failure(&format!("not a regular file: {}", path.display())));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || fs::read(path)? != bytes {
                return Err(failure("immutable object collision or corruption"));
            }
            sync_directory(
                path.parent()
                    .ok_or_else(|| failure("object has no parent"))?,
            )
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| failure("object has no parent"))?;
            let temporary = parent.join(format!(".tmp-{}", new_id()?));
            write_new(&temporary, bytes)?;
            fs::rename(temporary, path)?;
            sync_directory(parent)
        }
        Err(error) => Err(error.into()),
    }
}

fn names(path: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        names.push(
            entry?
                .file_name()
                .into_string()
                .map_err(|_| failure("non-UTF-8 store metadata name"))?,
        );
    }
    names.sort();
    Ok(names)
}

fn failure(message: &str) -> crate::Error {
    io::Error::other(message.to_owned()).into()
}

fn observe_manifest(tree: &Path, manifest: &Manifest) -> Result<Observed> {
    use std::os::unix::fs::MetadataExt;
    let mut observed = Observed::new();
    for (path, entry) in manifest {
        let metadata = fs::symlink_metadata(tree.join(path))?;
        observed.insert(
            path.clone(),
            content::Observation {
                id: entry.id.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        );
    }
    Ok(observed)
}

fn mutation_path(tree: &Path, relative: &str, allow_missing: bool) -> Result<PathBuf> {
    if relative.is_empty() || relative.contains('\0') {
        return Err(failure("mutation path must be a nonempty relative path"));
    }
    let path = Path::new(relative);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(failure("unsafe relative mutation path"));
    }
    let mut current = tree.to_owned();
    let count = path.components().count();
    for (index, component) in path.components().enumerate() {
        current.push(component);
        if index + 1 < count {
            require_directory(&current)?;
        } else {
            match fs::symlink_metadata(&current) {
                Ok(_) => {}
                Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(current)
}

fn apply_mutation(tree: &Path, observed: &mut Observed, mutation: &Mutation) -> Result<()> {
    match mutation {
        Mutation::Write { path, content } => {
            let target = mutation_path(tree, path, true)?;
            if let Ok(metadata) = fs::symlink_metadata(&target)
                && !metadata.is_file()
            {
                return Err(failure("write refuses symlinks and non-files"));
            }
            // This is already a private generation, so replacing a regular leaf
            // cannot expose partial data through a published workspace.
            let mut output = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(target)?;
            output.write_all(content)?;
            output.sync_all()?;
        }
        Mutation::Mkdir { path } => {
            fs::create_dir(mutation_path(tree, path, true)?)?;
        }
        Mutation::Remove { path } => {
            let target = mutation_path(tree, path, false)?;
            if fs::symlink_metadata(&target)?.is_dir() {
                fs::remove_dir_all(target)?;
            } else {
                fs::remove_file(target)?;
            }
            observed.retain(|key, _| key != path && !key.starts_with(&format!("{path}/")));
        }
        Mutation::Move { from, to } => {
            let source = mutation_path(tree, from, false)?;
            let target = mutation_path(tree, to, true)?;
            if fs::symlink_metadata(&target).is_ok() {
                return Err(failure("move target already exists"));
            }
            if to.starts_with(&format!("{from}/")) {
                return Err(failure("cannot move a directory inside itself"));
            }
            fs::rename(source, target)?;
            let moved: Vec<_> = observed
                .iter()
                .filter(|(key, _)| *key == from || key.starts_with(&format!("{from}/")))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            for (key, value) in moved {
                observed.remove(&key);
                observed.insert(format!("{to}{}", &key[from.len()..]), value);
            }
        }
        Mutation::Edit { path, old, new } => {
            if old.is_empty() {
                return Err(failure("edit old text must be nonempty"));
            }
            let target = mutation_path(tree, path, false)?;
            require_file(&target)?;
            let text = fs::read_to_string(&target)?;
            if text.match_indices(old).count() != 1 {
                return Err(failure("edit requires exactly one matching occurrence"));
            }
            let replacement = text.replacen(old, new, 1);
            let mut file = OpenOptions::new().write(true).truncate(true).open(target)?;
            file.write_all(replacement.as_bytes())?;
            file.sync_all()?;
        }
        Mutation::Copy { from, to } => {
            let source = mutation_path(tree, from, false)?;
            let target = mutation_path(tree, to, true)?;
            if fs::symlink_metadata(&target).is_ok() {
                return Err(failure("copy target already exists"));
            }
            if to.starts_with(&format!("{from}/")) {
                return Err(failure("cannot copy a directory inside itself"));
            }
            copy_subtree(&source, &target, &mut BTreeMap::new())?;
        }
        Mutation::Chmod { path, mode } => {
            let target = mutation_path(tree, path, false)?;
            if fs::symlink_metadata(&target)?.is_symlink() {
                return Err(failure("chmod refuses symlinks"));
            }
            if *mode & !0o777 != 0 {
                return Err(failure("only ordinary permission bits are supported"));
            }
            fs::set_permissions(target, fs::Permissions::from_mode(*mode))?;
        }
    }
    Ok(())
}

fn copy_subtree(
    source: &Path,
    target: &Path,
    links: &mut BTreeMap<(u64, u64), PathBuf>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        #[cfg(target_os = "macos")]
        copy_tree::set_symlink_permissions(target, metadata.mode() & 0o777)?;
        copy_tree::set_symlink_modified(
            target,
            metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
        )?;
    } else if metadata.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_subtree(&entry.path(), &target.join(entry.file_name()), links)?;
        }
        fs::set_permissions(target, fs::Permissions::from_mode(metadata.mode() & 0o777))?;
        File::open(target)?.set_times(fs::FileTimes::new().set_modified(metadata.modified()?))?;
        sync_directory(target)?;
    } else if metadata.is_file() {
        let identity = (metadata.dev(), metadata.ino());
        if let Some(previous) = links.get(&identity) {
            fs::hard_link(previous, target)?;
        } else {
            fs::copy(source, target)?;
            fs::set_permissions(target, fs::Permissions::from_mode(metadata.mode() & 0o777))?;
            let file = File::open(target)?;
            file.set_times(fs::FileTimes::new().set_modified(metadata.modified()?))?;
            file.sync_all()?;
            links.insert(identity, target.to_owned());
        }
    } else {
        return Err(failure("copy rejects special files"));
    }
    Ok(())
}

fn file_has_markers(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut buffer = [0u8; 65536];
    let mut prefix = [0u8; 7];
    let mut column = 0usize;
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            return Ok(false);
        }
        for byte in &buffer[..length] {
            if *byte == b'\n' {
                column = 0;
            } else if column < prefix.len() {
                prefix[column] = *byte;
                column += 1;
                if column == prefix.len() && matches!(&prefix, b"<<<<<<<" | b"=======" | b">>>>>>>")
                {
                    return Ok(true);
                }
            }
        }
    }
}

fn tree_has_markers(tree: &Path) -> Result<bool> {
    for entry in fs::read_dir(tree)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && tree_has_markers(&entry.path())? {
            return Ok(true);
        }
        if metadata.is_file() && file_has_markers(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolution_path(tree: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty() {
        return Ok(tree.to_owned());
    }
    if relative.contains('\0')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || Path::new(relative)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(failure("unsafe conflict path"));
    }
    let mut current = tree.to_owned();
    let components: Vec<_> = Path::new(relative).components().collect();
    for (index, part) in components.iter().enumerate() {
        current.push(part);
        if index + 1 < components.len() {
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Err(failure("conflict path has non-directory ancestor")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(tree.join(relative));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(current)
}

fn sync_native_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_native_tree(&entry?.path())?;
        }
        sync_directory(path)?;
    } else if metadata.is_file() {
        File::open(path)?.sync_all()?;
    } else if !metadata.file_type().is_symlink() {
        return Err(failure(
            "special file encountered while syncing native generation",
        ));
    }
    Ok(())
}
