//! Owned writes use the same revision graph and atomic State publication.
//!
//! Old journal generations are retained, never reset after an uncertain save.

use super::*;
use crate::writable::WritableCas;

impl Store {
    pub(super) fn writable_core(&self, workspace: &Workspace) -> Result<WritableCas> {
        #[cfg(test)]
        super::lifecycle_tests::CORE_OPENS.with(|count| count.set(count.get() + 1));
        let WorkspaceBackend::Writable { revision } = &workspace.backend else {
            return Err(failure("workspace does not own a writable CAS journal"));
        };
        let generation = self.root.join("native").join(&workspace.directory);
        require_file(&generation.join("writable-base"))?;
        if fs::read_to_string(generation.join("writable-base"))? != *revision {
            return Err(failure("writable generation pin mismatch"));
        }
        Ok(WritableCas::open(
            &self.objects(),
            &generation,
            &self.revision(revision)?.manifest,
            revision,
            &workspace.directory,
        )?)
    }

    pub(super) fn writable_generation_with_manifest(
        &self,
        revision: &str,
        manifest: &Manifest,
    ) -> Result<(String, Observed, WorkspaceBackend)> {
        let directory = self.new_native()?;
        let generation = self.root.join("native").join(&directory);
        platform::create_dir(&generation.join("tree"), 0o700)?;
        // The pin precedes publication and also protects abandoned preparations.
        write_new(&generation.join("writable-base"), revision.as_bytes())?;
        let core =
            WritableCas::create(&self.objects(), &generation, manifest, revision, &directory)?;
        core.sync()?;
        drop(core);
        sync_directory(&generation)?;
        Ok((
            directory,
            Observed::new(),
            WorkspaceBackend::Writable {
                revision: revision.into(),
            },
        ))
    }

    pub(super) fn detached_writable(
        &self,
        manifest: &Manifest,
    ) -> Result<(String, Observed, WorkspaceBackend)> {
        let revision = self.put_revision(&Revision {
            parents: vec![],
            clock: 1,
            message: "retained writable base".into(),
            manifest: manifest.clone(),
        })?;
        self.writable_generation_with_manifest(&revision, manifest)
    }

    /// Creates an owned CAS workspace without requiring a filesystem mount.
    /// Configure the executable and explicitly mount it for ordinary Linux tools.
    pub fn import_writable(&self, source: &Path, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        require_directory(source)?;
        let source = source.canonicalize()?;
        if source.starts_with(&self.root) || self.root.starts_with(&source) {
            return Err(failure("source and store must not contain one another"));
        }
        let (manifest, _) = content::capture(&source, &self.objects(), &Observed::new())?;
        let revision = self.put_revision(&Revision {
            parents: vec![],
            clock: 1,
            message: format!("import {name}"),
            manifest,
        })?;
        self.restore_writable_locked(&_lock, &mut state, &revision, name, name, true)
    }

    pub fn checkout_writable(&self, branch: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, false)?;
        let revision = branch_head(&state, branch, None)?;
        self.restore_writable_locked(&_lock, &mut state, &revision, branch, name, false)
    }

    pub fn restore_writable(&self, reference: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let (revision, data) = self.resolve_revision(&state, reference)?;
        self.restore_writable_manifest_locked(
            &_lock,
            &mut state,
            &revision,
            &data.manifest,
            name,
            name,
            true,
        )
    }

    pub(super) fn restore_writable_locked(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        revision: &str,
        branch: &str,
        name: &str,
        new_branch: bool,
    ) -> Result<WorkspaceResult> {
        let data = self.revision(revision)?;
        self.restore_writable_manifest_locked(
            _lock,
            state,
            revision,
            &data.manifest,
            branch,
            name,
            new_branch,
        )
    }

    #[allow(clippy::too_many_arguments)] // Explicit StoreLock is the observation access proof.
    fn restore_writable_manifest_locked(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        revision: &str,
        manifest: &Manifest,
        branch: &str,
        name: &str,
        new_branch: bool,
    ) -> Result<WorkspaceResult> {
        let (directory, observed, backend) =
            self.writable_generation_with_manifest(revision, manifest)?;
        let workspace = Workspace {
            backend,
            directory,
            observed: self.put_observed(_lock, &observed)?,
            branch: branch.into(),
            base_revision: revision.into(),
            observation_epoch: None,
            pending: None,
        };
        if new_branch {
            state.branches.insert(branch.into(), revision.into());
        }
        state.workspaces.insert(name.into(), workspace.clone());
        self.save(_lock, state)?;
        Ok(self.workspace_result(name, &workspace))
    }

    pub fn fork_writable(&self, source: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let source = workspace(&state, source)?.clone();
        let _lease = self.lease(&source)?;
        self.fork_writable_source(&_lock, &mut state, &source, name)
    }

    pub(super) fn fork_writable_locked(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        source: &Workspace,
        manifest: &Manifest,
        name: &str,
    ) -> Result<WorkspaceResult> {
        let (directory, observed, backend) = self.detached_writable(manifest)?;
        let fork = Workspace {
            backend,
            directory,
            observed: self.put_observed(_lock, &observed)?,
            branch: name.into(),
            base_revision: source.base_revision.clone(),
            observation_epoch: None,
            pending: source.pending.clone(),
        };
        state
            .branches
            .insert(name.into(), source.base_revision.clone());
        state.workspaces.insert(name.into(), fork.clone());
        self.save(_lock, state)?;
        Ok(self.workspace_result(name, &fork))
    }

    pub(super) fn retain_generation(
        &self,
        state: &mut State,
        name: &str,
        workspace: &Workspace,
    ) -> Result<()> {
        let token = new_id()?;
        state.trash.insert(
            token.clone(),
            TrashItem {
                token,
                name: format!("generation-{name}"),
                revision: workspace.base_revision.clone(),
                workspace: Some(workspace.clone()),
            },
        );
        Ok(())
    }
}

impl Store {
    pub(super) fn writable_mount_state(&self, workspace: &Workspace) -> String {
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            match crate::writable_lifecycle::inspect(
                &self.root.join("native").join(&workspace.directory),
            ) {
                Ok(state) => format!("{state:?}").to_ascii_lowercase(),
                Err(error) => format!("broken: {error}"),
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "projection")))]
        {
            let _ = workspace;
            "unmounted".into()
        }
    }

    pub(super) fn mount_writable(&self, workspace: &Workspace, initialize: bool) -> Result<()> {
        let WorkspaceBackend::Writable { revision } = &workspace.backend else {
            return Err(failure("not a writable CAS workspace"));
        };
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            self.require_projection_support()?;
            let generation = self.root.join("native").join(&workspace.directory);
            use crate::projection::lifecycle::MountState;
            match crate::writable_lifecycle::inspect(&generation)? {
                MountState::Mounted => {
                    crate::writable_lifecycle::verify_binding(&generation, revision)
                }
                MountState::Partial => Err(failure(
                    "writable mount is partial; normal unmount is required",
                )),
                MountState::Unmounted => crate::writable_lifecycle::start(
                    &self.root,
                    revision,
                    &generation,
                    self.projection_executable.as_deref().ok_or_else(|| {
                        failure("writable mount requires a configured Sprig executable")
                    })?,
                    &self.revision(revision)?.manifest,
                    initialize,
                ),
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "projection")))]
        {
            let _ = (revision, initialize);
            Err(failure(
                "writable filesystem mount requires Linux and the projection feature; SDK operations remain available",
            ))
        }
    }

    pub(super) fn stop_writable(&self, workspace: &Workspace) -> Result<()> {
        self.stop_writable_core(workspace).map(drop)
    }

    // Keep the acquired generation lease through capture/sync instead of
    // dropping it and rebuilding the same replay state immediately afterward.
    // The ordinary unmount and checked sync barrier are unchanged.
    pub(super) fn stop_writable_core(&self, workspace: &Workspace) -> Result<WritableCas> {
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            let generation = self.root.join("native").join(&workspace.directory);
            if crate::writable_lifecycle::inspect(&generation)?
                != crate::projection::lifecycle::MountState::Unmounted
            {
                crate::writable_lifecycle::stop(&generation)?;
            }
        }
        // A disappearing mount is not proof that the daemon released its journal.
        // A bounded retry acquires the actual exclusive lock, never a PID guess.
        let started = std::time::Instant::now();
        loop {
            match self.writable_core(workspace) {
                Ok(core) => {
                    core.sync()?;
                    return Ok(core);
                }
                Err(error)
                    if started.elapsed() < std::time::Duration::from_secs(5)
                        && error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::WouldBlock) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "projection"))]
    pub fn serve_writable(root: &Path, revision: &str, generation: &str) -> Result<()> {
        validate_name(generation)?;
        if !valid_hash(revision) {
            return Err(failure("invalid writable base revision"));
        }
        require_directory(root)?;
        let store = Store {
            root: root.canonicalize()?,
            projection_executable: None,
            observation_budget: ObservationBudget::default(),
        };
        store.state()?;
        let workspace = Workspace {
            backend: WorkspaceBackend::Writable {
                revision: revision.into(),
            },
            branch: "serving".into(),
            base_revision: revision.into(),
            directory: generation.into(),
            observed: observations::empty_reference(),
            observation_epoch: None,
            pending: None,
        };
        let path = store.root.join("native").join(generation);
        let startup = crate::writable_lifecycle::take_start_lock(&path)?;
        let core = store.writable_core(&workspace)?;
        let ready = Box::new(move || {
            use std::io::Write;
            let result = (|| {
                let mut output = std::io::stdout().lock();
                output.write_all(b"R")?;
                output.flush()
            })();
            drop(startup);
            result
        });
        let session = crate::writable_session::begin(&path, revision)?;
        let filesystem =
            crate::writable_fuse::WritableFuse::new(core, 0, 0, Some(ready), path.clone(), session);
        crate::writable_lifecycle::serve(filesystem, &path, revision)
    }
}

impl Store {
    pub(super) fn finish_writable_commit(
        &self,
        state: &State,
        name: &str,
        result: Result<CommitResult>,
        remount: bool,
    ) -> Result<CommitResult> {
        if !remount {
            return result.map_err(|error| {
                if state.workspaces.get(name).is_some_and(|workspace| matches!(workspace.backend, WorkspaceBackend::Writable { .. })) {
                    match self.state().and_then(|state| Ok(workspace(&state, name)?.clone())) {
                        Ok(current) => failure(&format!("{error}; authoritative revision {} generation {}; inspect before retrying", current.base_revision, current.directory)),
                        Err(read) => failure(&format!("{error}; authoritative state could not be read: {read}; publication outcome unknown")),
                    }
                } else { error }
            });
        }
        // Re-read publication after any error. Never replay the mutation/merge.
        let authoritative = self.state()?;
        let current = workspace(&authoritative, name)?;
        let mounted = self.ensure_mounted(current);
        match result {
            Ok(mut committed) => {
                committed.mount_error = mounted.err().map(|error| error.to_string());
                Ok(committed)
            }
            Err(error) => {
                let _ = state;
                if let Err(mount) = mounted {
                    return Err(failure(&format!(
                        "{error}; authoritative revision {} generation {}; remount failed: {mount}",
                        current.base_revision, current.directory
                    )));
                }
                Err(failure(&format!(
                    "{error}; authoritative revision {} generation {}; inspect before retrying",
                    current.base_revision, current.directory
                )))
            }
        }
    }

    pub(super) fn workspace_has_markers(
        &self,
        workspace: &Workspace,
        manifest: &Manifest,
    ) -> Result<bool> {
        if !matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            return tree_has_markers(&self.tree(&workspace.directory));
        }
        let reader = packed::Reader::open(&self.objects())?;
        for entry in manifest.values() {
            if matches!(entry.kind, EntryKind::File { .. }) {
                let bytes = content::read_file_from(entry, &reader)?;
                if has_marker_bytes(&bytes) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub(super) fn write_workspace_markers(
        &self,
        workspace: &Workspace,
        manifest: &Manifest,
        conflicts: &[merge::MergeConflict],
    ) -> Result<()> {
        if !matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            return self.write_conflict_markers(
                &self.tree(&workspace.directory),
                manifest,
                conflicts,
            );
        }
        let core = self.writable_core(workspace)?;
        let reader = packed::Reader::open(&self.objects())?;
        for conflict in conflicts {
            let (Some(base), Some(ours), Some(theirs)) =
                (&conflict.base, &conflict.ours, &conflict.theirs)
            else {
                continue;
            };
            let Some(entry) = manifest.get(&conflict.path) else {
                continue;
            };
            if entry.id != ours.id {
                continue;
            }
            let (Ok(base), Ok(ours), Ok(theirs)) = (
                content::read_file_from(base, &reader),
                content::read_file_from(ours, &reader),
                content::read_file_from(theirs, &reader),
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
                writable_write(&core, &conflict.path, markers.as_bytes())?;
            }
        }
        core.sync()?;
        Ok(())
    }

    pub(super) fn apply_writable_locked(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        name: &str,
        original: Workspace,
        mutations: &[Mutation],
        message: &str,
    ) -> Result<CommitResult> {
        let (manifest, _) = self.capture_workspace(_lock, &original)?;
        let (directory, observed, backend) = self.detached_writable(&manifest)?;
        let mut replacement = original.clone();
        replacement.directory = directory;
        replacement.backend = backend;
        replacement.observed = self.put_observed(_lock, &observed)?;
        let core = self.writable_core(&replacement)?;
        for mutation in mutations {
            writable_mutation(&core, mutation)?;
        }
        core.sync()?;
        let manifest = core.snapshot()?;
        drop(core);
        self.retain_generation(state, name, &original)?;
        if replacement.pending.is_some() {
            let result = CommitResult {
                committed: false,
                revision: replacement.base_revision.clone(),
                branch: replacement.branch.clone(),
                previous_head: Some(branch_head(state, &replacement.branch, None)?),
                mount_error: None,
            };
            state.workspaces.insert(name.into(), replacement);
            self.save(_lock, state)?;
            Ok(result)
        } else {
            self.commit_workspace(
                _lock,
                state,
                name,
                replacement,
                manifest,
                Observed::new(),
                message,
            )
        }
    }
}

pub(super) fn has_marker_bytes(bytes: &[u8]) -> bool {
    bytes.split(|byte| *byte == b'\n').any(|line| {
        line.starts_with(b"<<<<<<<") || line.starts_with(b"=======") || line.starts_with(b">>>>>>>")
    })
}

fn writable_parent(core: &WritableCas, path: &str) -> Result<(u64, String)> {
    platform::validate_relative_path(path)?;
    if path.is_empty() {
        return Err(failure("operation requires a non-root path"));
    }
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    Ok((core.lookup_path(parent)?.inode, name.into()))
}

fn writable_write(core: &WritableCas, path: &str, bytes: &[u8]) -> Result<()> {
    let node = match core.lookup_path(path) {
        Ok(node) => node,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let (parent, name) = writable_parent(core, path)?;
            core.create_file(parent, &name, 0o644)?
        }
        Err(error) => return Err(error.into()),
    };
    core.setattr(
        node.inode,
        crate::writable::SetAttr {
            size: Some(0),
            ..Default::default()
        },
    )?;
    let handle = core.open_handle(node.inode, true)?;
    let result = (|| -> Result<()> {
        for (index, chunk) in bytes.chunks(crate::writable::MAX_WRITE_BYTES).enumerate() {
            core.write(
                handle,
                (index * crate::writable::MAX_WRITE_BYTES) as u64,
                chunk,
            )?;
        }
        Ok(())
    })();
    core.release(handle)?;
    result
}

fn writable_mutation(core: &WritableCas, mutation: &Mutation) -> Result<()> {
    match mutation {
        Mutation::Write { path, content } => writable_write(core, path, content),
        Mutation::Mkdir { path } => {
            let (parent, name) = writable_parent(core, path)?;
            core.mkdir(parent, &name, 0o755)?;
            Ok(())
        }
        Mutation::Chmod { path, mode } => {
            let node = core.lookup_path(path)?;
            core.setattr(
                node.inode,
                crate::writable::SetAttr {
                    mode: Some(*mode),
                    ..Default::default()
                },
            )?;
            Ok(())
        }
        Mutation::Move { from, to } => {
            if core.lookup_path(to).is_ok() {
                return Err(failure("destination already exists"));
            }
            let (parent, name) = writable_parent(core, from)?;
            let (target, target_name) = writable_parent(core, to)?;
            core.rename(parent, &name, target, &target_name)?;
            Ok(())
        }
        Mutation::Remove { path } => {
            let node = core.lookup_path(path)?;
            if matches!(node.entry.kind, EntryKind::Directory) {
                let prefix = format!("{path}/");
                let mut children: Vec<_> = core
                    .paths()?
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(&prefix))
                    .collect();
                children.sort_by_key(|(key, _)| std::cmp::Reverse(key.matches('/').count()));
                for (key, child) in children {
                    let (parent, name) = writable_parent(core, &key)?;
                    if matches!(child.entry.kind, EntryKind::Directory) {
                        core.rmdir(parent, &name)?;
                    } else {
                        core.unlink(parent, &name)?;
                    }
                }
            }
            let (parent, name) = writable_parent(core, path)?;
            if matches!(node.entry.kind, EntryKind::Directory) {
                core.rmdir(parent, &name)?;
            } else {
                core.unlink(parent, &name)?;
            }
            Ok(())
        }
        Mutation::Edit { path, old, new } => {
            let node = core.lookup_path(path)?;
            let EntryKind::File { size, .. } = node.entry.kind else {
                return Err(failure("edit requires a regular file"));
            };
            if size > content::MAX_READ_FILE {
                return Err(failure("edit exceeds text size limit"));
            }
            let mut bytes = Vec::with_capacity(size as usize);
            while bytes.len() < size as usize {
                bytes.extend(core.read_inode(
                    node.inode,
                    bytes.len() as u64,
                    (size as usize - bytes.len()).min(crate::writable::MAX_WRITE_BYTES),
                )?);
            }
            let text = std::str::from_utf8(&bytes)?;
            if old.is_empty() || text.matches(old).count() != 1 {
                return Err(failure("edit requires exactly one match"));
            }
            writable_write(core, path, text.replacen(old, new, 1).as_bytes())
        }
        Mutation::Link { from, to } => {
            let source = core.lookup_path(from)?;
            let (parent, name) = writable_parent(core, to)?;
            core.link(source.inode, parent, &name)?;
            Ok(())
        }
        Mutation::Copy { from, to } => writable_copy(core, from, to),
    }
}

fn writable_copy(core: &WritableCas, from: &str, to: &str) -> Result<()> {
    if core.lookup_path(to).is_ok() {
        return Err(failure("destination already exists"));
    }
    if to.starts_with(&format!("{from}/")) {
        return Err(failure("copy destination cannot be within source"));
    }
    let root = core.lookup_path(from)?;
    let mut entries = vec![(from.to_owned(), root)];
    let prefix = format!("{from}/");
    entries.extend(
        core.paths()?
            .into_iter()
            .filter(|(path, _)| path.starts_with(&prefix)),
    );
    let mut finalize = Vec::new();
    let mut copied_inodes = BTreeMap::new();
    for (path, node) in entries {
        let target = format!("{to}{}", &path[from.len()..]);
        let (parent, name) = writable_parent(core, &target)?;
        let created = match node.entry.kind {
            EntryKind::Directory => core.mkdir(parent, &name, node.entry.mode)?,
            EntryKind::Symlink { target } => core.symlink(parent, &name, &target)?,
            EntryKind::File { size, .. } => {
                if let Some(inode) = copied_inodes.get(&node.inode) {
                    core.link(*inode, parent, &name)?;
                    continue;
                }
                let created = core.create_file(parent, &name, node.entry.mode)?;
                copied_inodes.insert(node.inode, created.inode);
                let handle = core.open_handle(created.inode, true)?;
                let mut offset = 0;
                while offset < size {
                    let bytes = core.read_inode(
                        node.inode,
                        offset,
                        (size - offset).min(crate::writable::MAX_WRITE_BYTES as u64) as usize,
                    )?;
                    if bytes.is_empty() {
                        return Err(failure("copy read ended before file size"));
                    }
                    core.write(handle, offset, &bytes)?;
                    offset += bytes.len() as u64;
                }
                core.release(handle)?;
                created
            }
        };
        finalize.push((created.inode, node.entry.mode, node.entry.modified_ns));
    }
    // Restore directory timestamps after all child namespace mutations.
    for (inode, mode, modified_ns) in finalize.into_iter().rev() {
        core.setattr(
            inode,
            crate::writable::SetAttr {
                mode: Some(mode),
                modified_ns: Some(modified_ns),
                ..Default::default()
            },
        )?;
    }
    Ok(())
}

impl Store {
    /// Inspect an unmounted journal without changing its bytes or branch head.
    /// Mounted views retain the exclusive journal lock and report busy.
    pub fn writable_status(&self, name: &str) -> Result<crate::WritableStatus> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let workspace = workspace(&state, name)?;
        let _lease = self.lease(workspace)?;
        let core = self.writable_core(workspace)?;
        let recovery = core.recovery_info()?;
        let stats = core.stats()?;
        Ok(crate::WritableStatus {
            dirty_paths: core.dirty_paths()?.into_iter().collect(),
            read_only: recovery.read_only,
            incomplete_tail: recovery.incomplete_tail,
            verified_journal_bytes: recovery.verified_bytes,
            journal_bytes: stats.journal_bytes,
            journal_limit: stats.journal_limit,
            logical_bytes: stats.logical_bytes,
            logical_limit: stats.logical_limit,
            nodes: stats.nodes,
            node_limit: stats.node_limit,
        })
    }
}

impl Store {
    /// Explicitly recover the verified prefix into a separate workspace. The
    /// original journal, including every incomplete byte, remains untouched.
    pub fn recover_writable(&self, source: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let source = workspace(&state, source)?.clone();
        let _lease = self.lease(&source)?;
        self.verify_observed(&_lock, &source.observed)?;
        let remount = self.mount_state(&source) == "mounted";
        let result = (|| {
            self.stop_writable(&source)?;
            let core = self.writable_core(&source)?;
            core.sync()?;
            let manifest = core.snapshot()?;
            drop(core);
            self.fork_writable_locked(&_lock, &mut state, &source, &manifest, name)
        })();
        self.finish_writable_view(&source, result, remount)
    }
}

impl Store {
    pub(super) fn finish_writable_view(
        &self,
        source: &Workspace,
        result: Result<WorkspaceResult>,
        remount: bool,
    ) -> Result<WorkspaceResult> {
        if !remount {
            return result;
        }
        let mounted = self.ensure_mounted(source);
        match result {
            Ok(mut value) => {
                value.mount_error = mounted.err().map(|error| {
                    format!(
                        "source generation {} could not be remounted: {error}",
                        source.directory
                    )
                });
                Ok(value)
            }
            Err(error) => {
                let state = self.state()?;
                let current = state
                    .workspaces
                    .values()
                    .find(|workspace| workspace.directory == source.directory);
                Err(failure(&format!(
                    "{error}; source generation {}; authoritative source revision {}; remount {}",
                    source.directory,
                    current
                        .map(|workspace| workspace.base_revision.as_str())
                        .unwrap_or(&source.base_revision),
                    mounted
                        .map(|_| "succeeded".to_string())
                        .unwrap_or_else(|error| error.to_string())
                )))
            }
        }
    }

    pub(super) fn fork_writable_source(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        source: &Workspace,
        name: &str,
    ) -> Result<WorkspaceResult> {
        let remount = matches!(source.backend, WorkspaceBackend::Writable { .. })
            && self.mount_state(source) == "mounted";
        let result = (|| {
            let (manifest, _) = self.capture_workspace(_lock, source)?;
            self.fork_writable_locked(_lock, state, source, &manifest, name)
        })();
        self.finish_writable_view(source, result, remount)
    }

    pub(super) fn resolve_writable_locked(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        name: &str,
        mut workspace: Workspace,
        paths: &[String],
    ) -> Result<WorkspaceResult> {
        let source = workspace.clone();
        let remount = self.mount_state(&workspace) == "mounted";
        let result = (|| {
            let (manifest, _) = self.capture_workspace(_lock, &workspace)?;
            let pending = workspace
                .pending
                .as_mut()
                .ok_or_else(|| failure("workspace has no pending merge"))?;
            if paths.is_empty() {
                return Err(failure("explicit conflict paths are required"));
            }
            let reader = packed::Reader::open(&self.objects())?;
            for path in paths {
                if !pending
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.path == *path)
                {
                    return Err(failure("path is not an unresolved conflict"));
                }
                if let Some(entry) = manifest.get(path)
                    && matches!(entry.kind, EntryKind::File { .. })
                    && has_marker_bytes(&content::read_file_from(entry, &reader)?)
                {
                    return Err(failure(
                        "unresolved_markers: remove marker lines before resolving",
                    ));
                }
            }
            pending
                .conflicts
                .retain(|conflict| !paths.contains(&conflict.path));
            state.workspaces.insert(name.into(), workspace.clone());
            self.save(_lock, state)?;
            Ok(self.workspace_result(name, &workspace))
        })();
        let mut value = self.finish_writable_view(&source, result, remount)?;
        value.mount_state = self.mount_state(&workspace);
        Ok(value)
    }
}
