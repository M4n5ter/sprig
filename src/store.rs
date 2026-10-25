//! One atomic state file publishes branch heads and workspace generations together.

mod collection;
#[cfg(test)]
mod lifecycle_tests;
mod observation_roots;
#[cfg(test)]
mod observation_tests;
mod storage;
mod writable;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::content::{self, EntryKind, Manifest, Observed};
use crate::copy_tree::{self, CopyMode};
use crate::manifest_pages;
use crate::merge::{self, MergeStrategy};
use crate::metadata;
use crate::model::{
    BranchResult, Change, CommitResult, DoctorReport, GcPlan, Listing, MergeOutcome, Mutation,
    Revision, RevisionSummary, State, Stats, Status, TrashItem, Workspace, WorkspaceBackend,
    WorkspaceResult,
};
use crate::observations::{self, Meter, ObservationBudget, ObservationRef};
use crate::packed;
use crate::platform;
use crate::{Blame, Result};
use serde::{Deserialize, Serialize};

// Identifies the one supported layout; this is not a compatibility layer.
const STORAGE_SCHEMA: u32 = 14;
const AREAS: [&str; 6] = [
    "objects",
    "revisions",
    "native",
    "staging",
    "backups",
    "observations",
];

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRevision {
    parents: Vec<String>,
    clock: u64,
    message: String,
    manifest: manifest_pages::StoredManifest,
}

/// Handle to a local, single-user store. Methods acquire their own metadata lock;
/// callers may clone handles, while branch-head checks prevent stale commits.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    projection_executable: Option<PathBuf>,
    observation_budget: ObservationBudget,
}

struct StoreLock {
    _file: File,
    root: PathBuf,
    observations: RefCell<Meter>,
}

impl Store {
    /// Select explicit operation-local observation decoding limits. These are
    /// resource guards, not format limits; maintenance fails closed on exhaustion.
    pub fn with_observation_budget(mut self, budget: ObservationBudget) -> Self {
        self.observation_budget = budget;
        self
    }

    fn observation_directory(&self) -> PathBuf {
        self.root.join("observations")
    }

    fn put_observed(&self, lock: &StoreLock, observed: &Observed) -> Result<ObservationRef> {
        if lock.root != self.root {
            return Err(failure("observation lock belongs to another store"));
        }
        require_directory(&self.observation_directory())?;
        fault("observations-before-put")?;
        let reference = observations::put(
            &self.observation_directory(),
            observed,
            &mut lock.observations.borrow_mut(),
        )?;
        fault("observations-after-put")?;
        Ok(reference)
    }

    fn read_observed(&self, lock: &StoreLock, reference: &ObservationRef) -> Result<Observed> {
        if lock.root != self.root {
            return Err(failure("observation lock belongs to another store"));
        }
        observations::read(
            &self.observation_directory(),
            reference,
            &mut lock.observations.borrow_mut(),
        )
    }

    fn verify_observed(&self, lock: &StoreLock, reference: &ObservationRef) -> Result<()> {
        if lock.root != self.root {
            return Err(failure("observation lock belongs to another store"));
        }
        observations::verify(
            &self.observation_directory(),
            reference,
            &mut lock.observations.borrow_mut(),
        )
    }

    /// Configure the installed Sprig executable used for foreground FUSE workers.
    /// Library callers opt in explicitly; no helper or system setting is installed.
    pub fn with_projection_executable(mut self, executable: PathBuf) -> Self {
        self.projection_executable = Some(executable);
        self
    }

    /// Read only immutable data for a FUSE worker. Does not acquire the metadata
    /// lock: its parent holds that lock while waiting for mount readiness.
    #[cfg(all(target_os = "linux", feature = "projection"))]
    pub fn projection_snapshot(
        root: &Path,
        revision: &str,
        generation: &str,
    ) -> Result<(PathBuf, Manifest, PathBuf)> {
        validate_name(generation)?;
        require_directory(root)?;
        let store = Self {
            root: root.canonicalize()?,
            projection_executable: None,
            observation_budget: ObservationBudget::default(),
        };
        store.state()?;
        let path = store.root.join("native").join(generation);
        require_directory(&path)?;
        require_file(&path.join("lease"))?;
        require_file(&path.join("projection-base"))?;
        if fs::read_to_string(path.join("projection-base"))? != revision {
            return Err(failure("projection generation pin mismatch"));
        }
        let data = store.revision(revision)?;
        Ok((store.objects(), data.manifest, path))
    }

    pub fn mount_workspace(&self, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let workspace = workspace(&state, name)?;
        let _lease = self.lease(workspace)?;
        self.ensure_mounted(workspace)?;
        Ok(self.workspace_result(name, workspace))
    }

    /// Normal unmount only. A busy external descriptor never forces detachment.
    pub fn unmount_workspace(&self, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let workspace = workspace(&state, name)?;
        let _lease = self.lease(workspace)?;
        self.stop_projection(workspace)?;
        Ok(self.workspace_result(name, workspace))
    }

    pub fn unmount_generation(&self, generation: &str) -> Result<()> {
        validate_name(generation)?;
        let _lock = self.lock()?;
        let _state = self.state()?;
        let path = self.root.join("native").join(generation);
        require_directory(&path)?;
        let owned = path.join("writable-base").try_exists()?;
        let pin = path.join(if owned {
            "writable-base"
        } else {
            "projection-base"
        });
        require_file(&pin)?;
        let revision = fs::read_to_string(pin)?;
        if !valid_hash(&revision) {
            return Err(failure("invalid projection pin"));
        }
        let workspace = Workspace {
            backend: if owned {
                WorkspaceBackend::Writable {
                    revision: revision.clone(),
                }
            } else {
                WorkspaceBackend::Projection {
                    revision: revision.clone(),
                }
            },
            branch: "retained".into(),
            base_revision: revision,
            directory: generation.into(),
            observed: observations::empty_reference(),
            observation_epoch: None,
            pending: None,
        };
        let _lease = self.lease(&workspace)?;
        self.stop_projection(&workspace)
    }

    fn mount_state(&self, workspace: &Workspace) -> String {
        if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            return self.writable_mount_state(workspace);
        }
        if matches!(workspace.backend, WorkspaceBackend::Native) {
            return "native".into();
        }
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            match crate::projection::lifecycle::inspect(
                &self.root.join("native").join(&workspace.directory),
            ) {
                Ok(state) => format!("{state:?}").to_ascii_lowercase(),
                Err(error) => format!("broken: {error}"),
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "projection")))]
        {
            "unsupported".into()
        }
    }

    fn ensure_mounted(&self, workspace: &Workspace) -> Result<()> {
        self.ensure_mounted_inner(workspace, false)
    }

    fn ensure_mounted_inner(&self, workspace: &Workspace, initialize: bool) -> Result<()> {
        if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            return self.mount_writable(workspace, initialize);
        }
        let WorkspaceBackend::Projection { revision } = &workspace.backend else {
            return require_directory(&self.tree(&workspace.directory));
        };
        let pin = self
            .root
            .join("native")
            .join(&workspace.directory)
            .join("projection-base");
        require_file(&pin)?;
        if fs::read_to_string(pin)? != *revision {
            return Err(failure("projection generation pin mismatch"));
        }
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            use crate::projection::lifecycle::{self, MountState};
            let generation = self.root.join("native").join(&workspace.directory);
            match lifecycle::inspect(&generation)? {
                MountState::Mounted => {
                    lifecycle::verify_binding(&generation, &self.revision(revision)?.manifest)
                }
                MountState::Partial => Err(failure(
                    "projection_partial: normal unmount is required before remount",
                )),
                MountState::Unmounted => lifecycle::start(
                    &self.root,
                    revision,
                    &generation,
                    self.projection_executable.as_deref().ok_or_else(|| {
                        failure("projection requires an explicitly configured Sprig executable")
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
                "projection requires Linux and the projection Cargo feature",
            ))
        }
    }

    fn stop_projection(&self, workspace: &Workspace) -> Result<()> {
        if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            return self.stop_writable(workspace);
        }
        if matches!(workspace.backend, WorkspaceBackend::Native) {
            return Ok(());
        }
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            let generation = self.root.join("native").join(&workspace.directory);
            if crate::projection::lifecycle::inspect(&generation)?
                == crate::projection::lifecycle::MountState::Mounted
            {
                sync_overlay_upper(&generation.join("upper"))?;
                sync_directory(&generation.join("work"))?;
                sync_directory(&generation)?;
            }
            crate::projection::lifecycle::stop(&generation)
        }
        #[cfg(not(all(target_os = "linux", feature = "projection")))]
        {
            Err(failure(
                "projection requires Linux and the projection Cargo feature",
            ))
        }
    }

    fn capture_workspace(
        &self,
        _lock: &StoreLock,
        workspace: &Workspace,
    ) -> Result<(Manifest, Observed)> {
        if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            self.verify_observed(_lock, &workspace.observed)?;
            let core = self.stop_writable_core(workspace)?;
            if core.recovery_info()?.read_only {
                return Err(failure(
                    "writable journal has an incomplete tail or abandoned mapped session; normal capture refused; inspect writable-status and explicitly recover-writable into a new workspace",
                ));
            }
            core.sync()?;
            return Ok((core.snapshot()?, Observed::new()));
        }
        self.ensure_mounted(workspace)?;
        match &workspace.backend {
            WorkspaceBackend::Writable { .. } => unreachable!("handled above"),
            WorkspaceBackend::Native => content::capture(
                &self.tree(&workspace.directory),
                &self.objects(),
                &self.read_observed(_lock, &workspace.observed)?,
            ),
            WorkspaceBackend::Projection { revision } => {
                #[cfg(all(target_os = "linux", feature = "projection"))]
                {
                    let generation = self.root.join("native").join(&workspace.directory);
                    let mut captured = crate::projection::capture::capture(
                        &self.revision(revision)?.manifest,
                        &self.objects(),
                        &generation.join("tree"),
                        &generation.join("upper"),
                        &generation.join("lower"),
                    )?;
                    let epoch = self.observation_epoch(workspace)?;
                    let observed = if epoch == workspace.observation_epoch {
                        self.read_observed(_lock, &workspace.observed)?
                    } else {
                        self.verify_observed(_lock, &workspace.observed)?;
                        Observed::new()
                    };
                    let previous = &observed;
                    captured.reconcile_identities(
                        &self.revision(&workspace.base_revision)?.manifest,
                        previous,
                    )?;
                    Ok((captured.manifest, captured.observed))
                }
                #[cfg(not(all(target_os = "linux", feature = "projection")))]
                {
                    let _ = revision;
                    Err(failure("projection backend unavailable"))
                }
            }
        }
    }

    fn observation_epoch(&self, workspace: &Workspace) -> Result<Option<String>> {
        if matches!(
            workspace.backend,
            WorkspaceBackend::Native | WorkspaceBackend::Writable { .. }
        ) {
            return Ok(None);
        }
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            Ok(Some(crate::projection::lifecycle::incarnation(
                &self.root.join("native").join(&workspace.directory),
            )?))
        }
        #[cfg(not(all(target_os = "linux", feature = "projection")))]
        {
            Err(failure("projection backend unavailable"))
        }
    }

    fn sync_workspace(&self, workspace: &Workspace) -> Result<()> {
        match workspace.backend {
            WorkspaceBackend::Writable { .. } => {
                self.stop_writable_core(workspace)?.sync()?;
                Ok(())
            }
            WorkspaceBackend::Native => sync_native_tree(&self.tree(&workspace.directory)),
            WorkspaceBackend::Projection { .. } => {
                self.ensure_mounted(workspace)?;
                let generation = self.root.join("native").join(&workspace.directory);
                sync_overlay_upper(&generation.join("upper"))?;
                sync_directory(&generation.join("work"))?;
                sync_directory(&generation)
            }
        }
    }

    fn projected_generation(&self, revision: &str) -> Result<(String, Observed, WorkspaceBackend)> {
        self.require_projection_support()?;
        let directory = self.new_native()?;
        let generation = self.root.join("native").join(&directory);
        write_new(&generation.join("projection-base"), revision.as_bytes())?;
        sync_directory(&generation)?;
        let backend = WorkspaceBackend::Projection {
            revision: revision.to_owned(),
        };
        let temporary = Workspace {
            backend: backend.clone(),
            branch: "preparing".into(),
            base_revision: revision.to_owned(),
            directory: directory.clone(),
            observed: observations::empty_reference(),
            observation_epoch: None,
            pending: None,
        };
        self.ensure_mounted_inner(&temporary, true)?;
        let observed =
            observe_manifest(&self.tree(&directory), &self.revision(revision)?.manifest)?;
        self.sync_workspace(&temporary)?;
        Ok((directory, observed, backend))
    }

    fn detached_projection(
        &self,
        manifest: &Manifest,
    ) -> Result<(String, Observed, WorkspaceBackend)> {
        let revision = self.put_revision(&Revision {
            parents: vec![],
            clock: 1,
            message: "retained projection base".into(),
            manifest: manifest.clone(),
        })?;
        self.projected_generation(&revision)
    }

    fn require_projection_support(&self) -> Result<()> {
        #[cfg(all(target_os = "linux", feature = "projection"))]
        {
            // SAFETY: geteuid only reads the current process credential.
            if unsafe { libc::geteuid() } != 0 {
                return Err(failure(
                    "projection currently requires root; no helper is installed",
                ));
            }
            if self.projection_executable.is_none() {
                return Err(failure("projection requires a configured Sprig executable"));
            }
            Ok(())
        }
        #[cfg(not(all(target_os = "linux", feature = "projection")))]
        {
            Err(failure(
                "projection requires Linux and the projection Cargo feature",
            ))
        }
    }

    pub fn import_projected(&self, source: &Path, name: &str) -> Result<WorkspaceResult> {
        self.require_projection_support()?;
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
        let (directory, observed, backend) = self.projected_generation(&revision)?;
        let mut workspace = Workspace {
            backend,
            branch: name.into(),
            base_revision: revision.clone(),
            directory,
            observed: self.put_observed(&_lock, &observed)?,
            observation_epoch: None,
            pending: None,
        };
        state.branches.insert(name.into(), revision);
        workspace.observation_epoch = self.observation_epoch(&workspace)?;
        state.workspaces.insert(name.into(), workspace.clone());
        self.save(&_lock, &mut state)?;
        Ok(self.workspace_result(name, &workspace))
    }

    pub fn checkout_projected(&self, branch: &str, name: &str) -> Result<WorkspaceResult> {
        self.restore_projected_inner(branch, name, false)
    }

    pub fn restore_projected(&self, reference: &str, name: &str) -> Result<WorkspaceResult> {
        self.restore_projected_inner(reference, name, true)
    }

    fn restore_projected_inner(
        &self,
        reference: &str,
        name: &str,
        new_branch: bool,
    ) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, new_branch)?;
        if !new_branch && !state.branches.contains_key(reference) {
            return Err(failure("branch not found"));
        }
        let revision = self.resolve(&state, reference)?;
        let (directory, observed, backend) = self.projected_generation(&revision)?;
        let branch = if new_branch { name } else { reference };
        let mut workspace = Workspace {
            backend,
            branch: branch.into(),
            base_revision: revision.clone(),
            directory,
            observed: self.put_observed(&_lock, &observed)?,
            observation_epoch: None,
            pending: None,
        };
        if new_branch {
            state.branches.insert(branch.into(), revision);
        }
        workspace.observation_epoch = self.observation_epoch(&workspace)?;
        state.workspaces.insert(name.into(), workspace.clone());
        self.save(&_lock, &mut state)?;
        Ok(self.workspace_result(name, &workspace))
    }

    pub fn init(path: &Path) -> Result<Self> {
        Self::init_with_compression(path, true)
    }

    /// Select the immutable payload encoding policy for a newly created store.
    pub fn init_with_compression(path: &Path, enabled: bool) -> Result<Self> {
        platform::create_dir(path, 0o700)?;
        let root = path.canonicalize()?;
        for area in AREAS {
            fs::create_dir(root.join(area))?;
            sync_directory(&root.join(area))?;
        }
        observations::put(
            &root.join("observations"),
            &Observed::new(),
            &mut Meter::default(),
        )?;
        write_new(
            &root.join("objects").join(".compression"),
            if enabled { b"auto\n" } else { b"never\n" },
        )?;
        sync_directory(&root.join("objects"))?;
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
        let store = Self {
            root,
            projection_executable: None,
            observation_budget: ObservationBudget::default(),
        };
        let _lock = store.lock()?;
        store.state()?;
        Ok(store)
    }

    /// Restore a previously saved metadata generation after validating every
    /// referenced revision and native tree. Current bytes are retained in staging.
    pub fn recover_state(path: &Path, generation: u64) -> Result<Status> {
        Self::recover_state_with_observation_budget(path, generation, ObservationBudget::default())
    }

    pub fn recover_state_with_observation_budget(
        path: &Path,
        generation: u64,
        budget: ObservationBudget,
    ) -> Result<Status> {
        require_directory(path)?;
        let store = Self {
            root: path.canonicalize()?,
            projection_executable: None,
            observation_budget: budget,
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
            store.verify_state_observations(&_lock, &restored)?;
            let roots = store.protected_roots_inner(&restored, false)?;
            let reader = packed::Reader::open(&store.objects())?;
            for id in store.ancestors(&roots)? {
                for hash in
                    content::referenced_chunks(&store.revision_with_reader(&id, &reader)?.manifest)
                {
                    reader.read(&hash)?;
                }
            }
            let protected_directories = store.protected_directories_inner(&restored, false)?;
            for directory in &protected_directories {
                require_directory(&store.root.join("native").join(directory))?;
                require_directory(&store.tree(directory))?;
            }
            // Active command leases must not be detached by metadata recovery.
            let mut held_leases = Vec::new();
            for directory in names(&store.root.join("native"))? {
                let native = store.root.join("native").join(&directory);
                require_directory(&native)?;
                let lease_path = native.join("lease");
                if !protected_directories.contains(&directory)
                    && platform::symlink_metadata(&lease_path)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                {
                    continue;
                }
                require_file(&lease_path)?;
                let lease = platform::open_lease(&lease_path)
                    .map_err(|error| failure(&format!("workspace_busy: {error}")))?;
                lease
                    .try_lock()
                    .map_err(|error| failure(&format!("workspace_busy: {error}")))?;
                held_leases.push(lease);
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
            let current_generation = current_json
                .as_ref()
                .and_then(|value| value.get("generation"))
                .and_then(serde_json::Value::as_u64);
            let current = current_json
                .and_then(|value| serde_json::from_value::<State>(value).ok())
                .filter(|state| validate_state(state).is_ok());
            let mut recovery_states = store.historical_states()?;
            recovery_states.push(restored.clone());
            if let Some(current) = &current {
                recovery_states.push(current.clone());
            }
            let mut checked = BTreeSet::new();
            for state in recovery_states {
                for workspace in state.workspaces.values().chain(
                    state
                        .trash
                        .values()
                        .filter_map(|item| item.workspace.as_ref()),
                ) {
                    if checked.insert(workspace.directory.clone()) {
                        store.stop_projection(workspace)?;
                    }
                }
            }
            let highest = store
                .historical_states()?
                .iter()
                .map(|state| state.generation)
                .chain(current_generation)
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
            fault("recovery-before-rename")?;
            platform::rename(&staged, &current_path)?;
            fault("recovery-after-rename")?;
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
        let mut workspace = Workspace {
            backend: WorkspaceBackend::Native,
            branch: name.to_owned(),
            base_revision: revision.clone(),
            directory,
            observed: self.put_observed(&_lock, &observed)?,
            observation_epoch: None,
            pending: None,
        };
        state.branches.insert(name.to_owned(), revision);
        workspace.observation_epoch = self.observation_epoch(&workspace)?;
        state.workspaces.insert(name.to_owned(), workspace.clone());
        self.save(&_lock, &mut state)?;
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
        if matches!(source.backend, WorkspaceBackend::Writable { .. }) && mode != CopyMode::Auto {
            return Err(failure(
                "native copy policy does not apply to a writable CAS fork",
            ));
        }
        if matches!(source.backend, WorkspaceBackend::Writable { .. }) {
            return self.fork_writable_source(&_lock, &mut state, &source, name);
        }
        if matches!(source.backend, WorkspaceBackend::Projection { .. }) {
            let (manifest, _) = self.capture_workspace(&_lock, &source)?;
            if mode != CopyMode::Auto {
                return Err(failure(
                    "native copy policy does not apply to a projected fork",
                ));
            }
            return self.fork_projected_locked(&_lock, &mut state, &source, &manifest, name, false);
        }
        let directory = self.new_native()?;
        // Reconcile against the checked source identities before rebinding to
        // destination inodes. Fork copies uncommitted bytes; it need not hash
        // and publish them to CAS before doing that same copy.
        let (copy_stats, copied) = copy_tree::copy_tree_with_identities(
            &self.tree(&source.directory),
            &self.tree(&directory),
            mode,
        )?;
        let observed =
            content::observe_copied(&copied, &self.read_observed(&_lock, &source.observed)?)?;
        let mut fork = Workspace {
            backend: WorkspaceBackend::Native,
            branch: name.to_owned(),
            base_revision: source.base_revision.clone(),
            directory,
            observed: self.put_observed(&_lock, &observed)?,
            observation_epoch: None,
            pending: source.pending.clone(),
        };
        state.branches.insert(name.to_owned(), source.base_revision);
        fork.observation_epoch = self.observation_epoch(&fork)?;
        state.workspaces.insert(name.to_owned(), fork.clone());
        self.save(&_lock, &mut state)?;
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
        let data = self.revision(&revision)?;
        self.restore_locked(&_lock, &mut state, &revision, &data, branch, name, false)
    }

    pub fn restore(&self, reference: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let (revision, data) = self.resolve_revision(&state, reference)?;
        self.restore_locked(&_lock, &mut state, &revision, &data, name, name, true)
    }

    #[allow(clippy::too_many_arguments)] // Explicit StoreLock is the observation access proof.
    fn restore_locked(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        revision: &str,
        data: &Revision,
        branch: &str,
        name: &str,
        new_branch: bool,
    ) -> Result<WorkspaceResult> {
        let directory = self.new_native()?;
        content::materialize(&data.manifest, &self.objects(), &self.tree(&directory))?;
        let observed = observe_manifest(&self.tree(&directory), &data.manifest)?;
        let mut workspace = Workspace {
            backend: WorkspaceBackend::Native,
            branch: branch.to_owned(),
            base_revision: revision.to_owned(),
            directory,
            observed: self.put_observed(_lock, &observed)?,
            observation_epoch: None,
            pending: None,
        };
        if new_branch {
            state
                .branches
                .insert(branch.to_owned(), revision.to_owned());
        }
        workspace.observation_epoch = self.observation_epoch(&workspace)?;
        state.workspaces.insert(name.to_owned(), workspace.clone());
        self.save(_lock, state)?;
        Ok(self.workspace_result(name, &workspace))
    }

    pub fn fork_projected(&self, source: &str, name: &str) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        fresh_name(&state, name, true)?;
        let source = workspace(&state, source)?.clone();
        let _lease = self.lease(&source)?;
        let remount = matches!(source.backend, WorkspaceBackend::Writable { .. })
            && self.mount_state(&source) == "mounted";
        let result = (|| {
            let (manifest, _) = self.capture_workspace(&_lock, &source)?;
            self.fork_projected_locked(&_lock, &mut state, &source, &manifest, name, false)
        })();
        self.finish_writable_view(&source, result, remount)
    }

    fn fork_projected_locked(
        &self,
        _lock: &StoreLock,
        state: &mut State,
        source: &Workspace,
        manifest: &Manifest,
        name: &str,
        stop_source: bool,
    ) -> Result<WorkspaceResult> {
        let (directory, observed, backend) = self.detached_projection(manifest)?;
        if stop_source {
            self.stop_projection(source)?;
        }
        let mut fork = Workspace {
            backend,
            branch: name.into(),
            base_revision: source.base_revision.clone(),
            directory,
            observed: self.put_observed(_lock, &observed)?,
            observation_epoch: None,
            pending: source.pending.clone(),
        };
        state
            .branches
            .insert(name.into(), source.base_revision.clone());
        fork.observation_epoch = self.observation_epoch(&fork)?;
        state.workspaces.insert(name.into(), fork.clone());
        self.save(_lock, state)?;
        Ok(self.workspace_result(name, &fork))
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
        let remount = matches!(workspace.backend, WorkspaceBackend::Writable { .. })
            && self.mount_state(&workspace) == "mounted";
        let result = (|| {
            let (manifest, observed) = self.capture_workspace(&_lock, &workspace)?;
            self.commit_workspace(
                &_lock, &mut state, name, workspace, manifest, observed, message,
            )
        })();
        self.finish_writable_commit(&state, name, result, remount)
    }

    #[allow(clippy::too_many_arguments)] // Explicit StoreLock is the observation access proof.
    fn commit_workspace(
        &self,
        _lock: &StoreLock,
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
        if workspace.pending.is_some() && self.workspace_has_markers(&workspace, &manifest)? {
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
            let (directory, observed, backend) =
                if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
                    self.detached_writable(&merged.manifest)?
                } else if matches!(workspace.backend, WorkspaceBackend::Projection { .. }) {
                    self.detached_projection(&merged.manifest)?
                } else {
                    let directory = self.new_native()?;
                    content::materialize(
                        &merged.manifest,
                        &self.objects(),
                        &self.tree(&directory),
                    )?;
                    let observed = observe_manifest(&self.tree(&directory), &merged.manifest)?;
                    (directory, observed, WorkspaceBackend::Native)
                };
            self.stop_projection(&workspace)?;
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
            workspace.backend = backend;
            workspace.base_revision = previous.clone();
            workspace.observed = self.put_observed(_lock, &observed)?;
            if !merged.conflicts.is_empty() {
                self.write_workspace_markers(&workspace, &merged.manifest, &merged.conflicts)?;
                workspace.pending = Some(crate::PendingMerge {
                    candidate_revision: candidate,
                    conflicts: merged.conflicts,
                });
                self.sync_workspace(&workspace)?;
                workspace.observation_epoch = self.observation_epoch(&workspace)?;
                state.workspaces.insert(name.to_owned(), workspace);
                self.save(_lock, state)?;
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
        let (revision, final_manifest) = if parent.manifest == final_manifest {
            (previous.clone(), final_manifest)
        } else {
            let data = Revision {
                parents,
                clock: clock
                    .checked_add(1)
                    .ok_or_else(|| failure("logical clock exhausted"))?,
                message: message.to_owned(),
                manifest: final_manifest,
            };
            (self.put_revision(&data)?, data.manifest)
        };
        if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            self.retain_generation(state, name, &workspace)?;
            let (directory, observed, backend) =
                self.writable_generation_with_manifest(&revision, &final_manifest)?;
            workspace.directory = directory;
            workspace.backend = backend;
            final_observed = observed;
        }
        workspace.base_revision = revision.clone();
        workspace.observed = self.put_observed(_lock, &final_observed)?;
        workspace.pending = None;
        let branch = workspace.branch.clone();
        state.branches.insert(branch.clone(), revision.clone());
        self.sync_workspace(&workspace)?;
        workspace.observation_epoch = self.observation_epoch(&workspace)?;
        state.workspaces.insert(name.to_owned(), workspace);
        self.save(_lock, state)?;
        Ok(CommitResult {
            committed: true,
            revision,
            branch,
            previous_head: Some(previous),
            mount_error: None,
        })
    }

    fn write_conflict_markers(
        &self,
        tree: &Path,
        manifest: &Manifest,
        conflicts: &[merge::MergeConflict],
    ) -> Result<()> {
        let reader = packed::Reader::open(&self.objects())?;
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
                let Ok(target) = mutation_path(tree, &conflict.path, false) else {
                    continue;
                };
                if require_file(&target).is_err() {
                    continue;
                }
                fs::write(&target, markers.as_bytes())?;
                platform::sync_file(&target)?;
            }
        }
        Ok(())
    }

    pub fn resolve_conflicts(&self, name: &str, paths: &[String]) -> Result<WorkspaceResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        let mut workspace = workspace(&state, name)?.clone();
        let _lease = self.lease(&workspace)?;
        if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
            return self.resolve_writable_locked(&_lock, &mut state, name, workspace, paths);
        }
        self.ensure_mounted(&workspace)?;
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
            if platform::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_file())
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
        self.save(&_lock, &mut state)?;
        Ok(self.workspace_result(name, &workspace))
    }

    /// Creates only a committed CAS reference, never a materialized working tree.
    pub fn branch(&self, name: &str, reference: &str) -> Result<BranchResult> {
        let _lock = self.lock()?;
        let mut state = self.state()?;
        validate_name(name)?;
        if state.branches.contains_key(name) {
            return Err(failure("branch already exists"));
        }
        let revision = self.resolve(&state, reference)?;
        state.branches.insert(name.to_owned(), revision.clone());
        self.save(&_lock, &mut state)?;
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
            self.save(&_lock, &mut state)?;
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
        self.save(&_lock, &mut state)?;
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
        let (_, target) = self.resolve_revision(&state, reference)?;
        let old_revision = self.revision(&old)?;
        let revision = self.put_revision(&Revision {
            parents: vec![old.clone()],
            clock: old_revision
                .clock
                .checked_add(1)
                .ok_or_else(|| failure("logical clock exhausted"))?,
            message: message.to_owned(),
            manifest: target.manifest,
        })?;
        state.branches.insert(branch.to_owned(), revision.clone());
        self.save(&_lock, &mut state)?;
        Ok(CommitResult {
            committed: true,
            revision,
            branch: branch.to_owned(),
            previous_head: Some(old),
            mount_error: None,
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
        if matches!(original.backend, WorkspaceBackend::Writable { .. }) {
            let remount = self.mount_state(&original) == "mounted";
            let result =
                self.apply_writable_locked(&_lock, &mut state, name, original, mutations, message);
            return self.finish_writable_commit(&state, name, result, remount);
        }
        let (original_manifest, _) = self.capture_workspace(&_lock, &original)?;
        let directory = self.new_native()?;
        let tree = self.tree(&directory);
        copy_tree::copy_tree(&self.tree(&original.directory), &tree, CopyMode::Auto)?;
        let mut observed = observe_manifest(&tree, &original_manifest)?;
        for mutation in mutations {
            apply_mutation(&tree, &mut observed, mutation)?;
        }
        let (manifest, observed) = content::capture(&tree, &self.objects(), &observed)?;
        let mut replacement = original.clone();
        if matches!(original.backend, WorkspaceBackend::Projection { .. }) {
            let (projected, projected_observed, backend) = self.detached_projection(&manifest)?;
            replacement.directory = projected;
            replacement.backend = backend;
            replacement.observed = self.put_observed(&_lock, &projected_observed)?;
            // This unpublished SDK preparation is reproducible from the retained
            // original and the already durable projected snapshot. Never remove
            // user-visible generations or anything after an uncertain save.
            fs::remove_dir_all(self.root.join("native").join(&directory))?;
            sync_directory(&self.root.join("native"))?;
            self.stop_projection(&original)?;
        } else {
            replacement.directory = directory;
        }
        let observed = if matches!(replacement.backend, WorkspaceBackend::Projection { .. }) {
            self.read_observed(&_lock, &replacement.observed)?
        } else {
            observed
        };
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
            replacement.observed = self.put_observed(&_lock, &observed)?;
            let result = CommitResult {
                committed: false,
                revision: replacement.base_revision.clone(),
                branch: replacement.branch.clone(),
                previous_head: Some(branch_head(&state, &replacement.branch, expected)?),
                mount_error: None,
            };
            self.sync_workspace(&replacement)?;
            replacement.observation_epoch = self.observation_epoch(&replacement)?;
            state.workspaces.insert(name.to_owned(), replacement);
            self.save(&_lock, &mut state)?;
            return Ok(result);
        }
        self.commit_workspace(
            &_lock,
            &mut state,
            name,
            replacement,
            manifest,
            observed,
            message,
        )
    }

    pub fn exec(&self, name: &str, program: &str, arguments: &[String]) -> Result<i32> {
        let (workspace, lease) = {
            let _lock = self.lock()?;
            let state = self.state()?;
            let workspace = workspace(&state, name)?.clone();
            let lease = self.lease(&workspace)?;
            self.ensure_mounted(&workspace)?;
            (workspace, lease)
        };
        // This is a coordination lease, NOT a security sandbox. Keep the file
        // alive while waiting, but release the metadata lock for other workspaces.
        #[cfg(unix)]
        use std::os::fd::AsRawFd;
        #[cfg(unix)]
        use std::os::unix::process::CommandExt;
        #[cfg(unix)]
        let descriptor = lease.as_raw_fd();
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(self.tree(&workspace.directory));
        // SAFETY: the child hook performs only async-signal-safe fcntl calls.
        // The live lease fd is inherited across exec so killing this supervising
        // process cannot silently unlock a child that is still writing.
        #[cfg(unix)]
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
        #[cfg(unix)]
        let status = command.status()?;
        #[cfg(windows)]
        let status = platform::spawn_leased(&mut command, &lease)?.wait()?;
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
                self.stop_projection(&workspace)?;
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
        self.save(&_lock, &mut state)?;
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
            None => {
                let data = self.revision(&item.revision)?;
                self.restore_locked(&_lock, &mut state, &item.revision, &data, name, name, true)
            }
            Some(old) => {
                let _lease = self.lease(&old)?;
                if matches!(old.backend, WorkspaceBackend::Writable { .. }) {
                    if mode != CopyMode::Auto {
                        return Err(failure(
                            "native copy policy does not apply to writable recovery",
                        ));
                    }
                    return self.fork_writable_source(&_lock, &mut state, &old, name);
                }
                let previously_mounted = self.mount_state(&old) == "mounted";
                let (manifest, _) = self.capture_workspace(&_lock, &old)?;
                if matches!(old.backend, WorkspaceBackend::Projection { .. }) {
                    if mode != CopyMode::Auto {
                        return Err(failure(
                            "native copy policy does not apply to projected recovery",
                        ));
                    }
                    return self.fork_projected_locked(
                        &_lock,
                        &mut state,
                        &old,
                        &manifest,
                        name,
                        !previously_mounted,
                    );
                }
                let directory = self.new_native()?;
                let copy_stats =
                    copy_tree::copy_tree(&self.tree(&old.directory), &self.tree(&directory), mode)?;
                let observed = observe_manifest(&self.tree(&directory), &manifest)?;
                let mut workspace = Workspace {
                    backend: WorkspaceBackend::Native,
                    branch: name.to_owned(),
                    base_revision: old.base_revision.clone(),
                    directory,
                    observed: self.put_observed(&_lock, &observed)?,
                    observation_epoch: None,
                    pending: old.pending.clone(),
                };
                state.branches.insert(name.to_owned(), old.base_revision);
                workspace.observation_epoch = self.observation_epoch(&workspace)?;
                state.workspaces.insert(name.to_owned(), workspace.clone());
                self.save(&_lock, &mut state)?;
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
        let reader = packed::Reader::open(&self.objects())?;
        for id in self.ancestors(&roots)? {
            let revision = self.revision_with_reader(&id, &reader)?;
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
        let before = self.resolve_revision(&state, from)?.1.manifest;
        let after = self.resolve_revision(&state, to)?.1.manifest;
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
        let before = self.resolve_revision(&state, from)?.1.manifest;
        let after = self.resolve_revision(&state, to)?.1.manifest;
        let before_ids: BTreeMap<_, _> = before
            .iter()
            .map(|(path, entry)| (entry.id.as_str(), (path, entry)))
            .collect();
        let reader = packed::Reader::open(&self.objects())?;
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
            result.push(self.file_diff(path, old, Some(entry), &reader)?);
        }
        for (path, entry) in &before {
            if !seen.contains(path) && !after.values().any(|new| new.id == entry.id) {
                result.push(self.file_diff(path, Some((path, entry)), None, &reader)?);
            }
        }
        Ok(result)
    }

    fn file_diff(
        &self,
        path: &str,
        old: Option<(&String, &content::Entry)>,
        new: Option<&content::Entry>,
        reader: &packed::Reader,
    ) -> Result<crate::FileDiff> {
        let load = |entry: Option<&content::Entry>| -> Result<Option<String>> {
            match entry {
                None => Ok(Some(String::new())),
                Some(entry) if matches!(entry.kind, EntryKind::File { size, .. } if size <= content::MAX_READ_FILE) =>
                {
                    let bytes = content::read_file_from(entry, reader)?;
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
        let reader = packed::Reader::open(&self.objects())?;
        crate::blame::attribute(&revision, path, &self.objects(), |id| {
            self.revision_with_reader(id, &reader)
        })
    }

    pub fn doctor(&self) -> Result<DoctorReport> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let mut errors = Vec::new();
        if let Err(error) = self.verify_observation_roots(&_lock, &state) {
            errors.push(format!("observations: {error}"));
        }
        let mut reachability_complete = true;
        let roots = match self.protected_roots(&state) {
            Ok(roots) => roots,
            Err(error) => {
                errors.push(format!("retained roots: {error}"));
                reachability_complete = false;
                Vec::new()
            }
        };
        let reachable = match self.ancestors(&roots) {
            Ok(ids) => ids,
            Err(error) => {
                errors.push(error.to_string());
                reachability_complete = false;
                BTreeSet::new()
            }
        };
        let mut referenced = BTreeSet::new();
        let reader = packed::Reader::open(&self.objects());
        for id in &reachable {
            match reader
                .as_ref()
                .map_err(|error| failure(&error.to_string()))
                .and_then(|reader| self.revision_with_reader(id, reader))
            {
                Ok(revision) => {
                    referenced.extend(content::referenced_chunks(&revision.manifest));
                    referenced.extend(manifest_pages::referenced_chunks(
                        &self.stored_revision(id)?.manifest,
                    ));
                }
                Err(error) => errors.push(format!("revision {id}: {error}")),
            }
        }
        match &reader {
            Ok(reader) => {
                for hash in referenced {
                    if let Err(error) = reader.read(&hash) {
                        errors.push(format!("object {hash}: {error}"));
                    }
                }
            }
            Err(error) => errors.push(format!("object index: {error}")),
        }
        for (name, workspace) in &state.workspaces {
            if let Err(error) = require_directory(&self.tree(&workspace.directory)) {
                errors.push(format!("workspace {name}: {error}"));
            }
            if matches!(workspace.backend, WorkspaceBackend::Writable { .. }) {
                match self.writable_core(workspace) {
                    Ok(core) => match core.recovery_info() {
                        Ok(info) if info.incomplete_tail => errors.push(format!("workspace {name}: incomplete writable journal; explicit prefix recovery required")),
                        Ok(info) if info.read_only => errors.push(format!("workspace {name}: abandoned or failed mapped session; explicit journal recovery required")),
                        Ok(_) => {},
                        Err(error) => errors.push(format!("workspace {name}: {error}")),
                    },
                    Err(error) if error.downcast_ref::<io::Error>().is_some_and(|error| error.kind() == io::ErrorKind::WouldBlock) => {
                        errors.push(format!("workspace {name}: writable journal is busy; unmount for a complete doctor inspection"));
                    },
                    Err(error) => errors.push(format!("workspace {name}: {error}")),
                }
            }
            if let WorkspaceBackend::Projection { revision } = &workspace.backend {
                let generation = self.root.join("native").join(&workspace.directory);
                for area in ["upper", "work", "lower"] {
                    if let Err(error) = require_directory(&generation.join(area)) {
                        errors.push(format!("workspace {name} {area}: {error}"));
                    }
                }
                #[cfg(all(target_os = "linux", feature = "projection"))]
                {
                    let check = (|| -> Result<()> {
                        use crate::projection::lifecycle::{self, MountState};
                        match lifecycle::inspect(&generation)? {
                            MountState::Unmounted => Ok(()),
                            MountState::Partial => Err(failure("partial projection mount")),
                            MountState::Mounted => lifecycle::verify_binding(
                                &generation,
                                &self.revision(revision)?.manifest,
                            ),
                        }
                    })();
                    if let Err(error) = check {
                        errors.push(format!("workspace {name}: {error}"));
                    }
                }
                #[cfg(not(all(target_os = "linux", feature = "projection")))]
                {
                    let _ = revision;
                    errors.push(format!("workspace {name}: projection backend unavailable"));
                }
            }
        }
        let protected_directories = match self.protected_directories(&state) {
            Ok(directories) => directories,
            Err(error) => {
                errors.push(format!("retained directories: {error}"));
                reachability_complete = false;
                BTreeSet::new()
            }
        };
        let orphan_workspaces = names(&self.root.join("native"))?
            .into_iter()
            .filter(|name| reachability_complete && !protected_directories.contains(name))
            .collect();
        let orphan_revisions = names(&self.root.join("revisions"))?
            .into_iter()
            .filter_map(|name| name.strip_suffix(".json.zst").map(str::to_owned))
            .filter(|id| reachability_complete && !reachable.contains(id))
            .collect();
        Ok(DoctorReport {
            healthy: errors.is_empty(),
            errors,
            orphan_revisions,
            orphan_workspaces,
            staging_entries: names(&self.root.join("staging"))?,
            object_store_entries: names(&self.objects())?,
        })
    }

    /// A conservative dry-run only. State backups are roots, so neither dropped
    /// work nor previously published history silently becomes collectible.
    pub fn gc(&self) -> Result<GcPlan> {
        let _lock = self.lock()?;
        self.gc_locked(&_lock)
    }

    fn gc_locked(&self, _lock: &StoreLock) -> Result<GcPlan> {
        let state = self.state()?;
        let observation_plan = self.observation_plan(_lock, &state)?;
        let reachable = self.ancestors(&self.protected_roots(&state)?)?;
        let mut objects = BTreeSet::new();
        let reader = packed::Reader::open(&self.objects())?;
        for id in &reachable {
            objects.extend(content::referenced_chunks(
                &self.revision_with_reader(id, &reader)?.manifest,
            ));
            objects.extend(manifest_pages::referenced_chunks(
                &self.stored_revision(id)?.manifest,
            ));
        }
        let catalog = object_catalog(&self.objects(), &reader)?;
        let unreachable_objects: Vec<_> = catalog
            .keys()
            .filter(|hash| !objects.contains(*hash))
            .cloned()
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
        let locations = reader.locations();
        let mut reclaimable_bytes = 0;
        for hash in &unreachable_objects {
            if !locations.contains_key(hash) {
                reclaimable_bytes += catalog[hash];
            }
        }
        // A partially live pack cannot be reclaimed without rewriting it.
        let live_packs: BTreeSet<_> = locations
            .iter()
            .filter(|(hash, _)| objects.contains(*hash))
            .map(|(_, location)| &location.pack)
            .collect();
        for (pack, size) in reader.pack_sizes() {
            if !live_packs.contains(&pack) {
                reclaimable_bytes += size;
            }
        }
        let compaction_upper_bound_bytes = objects
            .iter()
            .try_fold(64 * 1024 * 1024_u64, |total, hash| {
                total.checked_add(catalog[hash])
            })
            .ok_or_else(|| failure("compaction size overflow"))?;
        Ok(GcPlan {
            plan_id: self.gc_fingerprint(&state, &objects, &observation_plan)?,
            compaction_upper_bound_bytes,
            dry_run: true,
            generation: state.generation,
            unreachable_objects,
            unreachable_revisions,
            unreachable_observations: observation_plan.unreachable,
            orphan_workspaces,
            reclaimable_bytes: reclaimable_bytes.checked_add(observation_plan.reclaimable_bytes).ok_or_else(|| failure("reclaimable size overflow"))?,
            note: "No files deleted. Heads, workspace bases, immutable lowers, trash, parents and backups remain protected. Apply the exact plan to compact and reclaim unreferenced payloads. Open readers defer deletion. Working generations and unknown files are never pruned.".into(),
        })
    }

    pub fn stats(&self) -> Result<Stats> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let revisions = self.ancestors(&self.protected_roots(&state)?)?;
        let reader = packed::Reader::open(&self.objects())?;
        let objects = object_catalog(&self.objects(), &reader)?;
        let object_bytes = objects.values().sum();
        let mut object_storage_bytes = 0_u64;
        #[cfg(unix)]
        let mut object_allocated_bytes = 0_u64;
        // Include the index, lock, orphan payloads and temporary files. This is
        // referenced allocation, not exclusive physical space on CoW volumes.
        for entry in fs::read_dir(self.objects())? {
            let path = entry?.path();
            let metadata = platform::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(failure("unexpected non-regular object store entry"));
            }
            object_storage_bytes = object_storage_bytes
                .checked_add(metadata.len())
                .ok_or_else(|| failure("object storage size overflow"))?;
            #[cfg(unix)]
            {
                object_allocated_bytes += metadata.blocks() * 512;
            }
        }
        let mut reachable_logical_bytes = 0;
        for id in &revisions {
            for entry in self.revision_with_reader(id, &reader)?.manifest.values() {
                if let EntryKind::File { size, .. } = entry.kind {
                    reachable_logical_bytes += size;
                }
            }
        }
        let storage = self.storage_usage()?;
        let store_logical_bytes = storage.values().map(|usage| usage.logical_bytes).sum();
        let store_allocated_bytes = storage.values().try_fold(0_u64, |total, usage| {
            total.checked_add(usage.allocated_bytes?)
        });
        Ok(Stats {
            payload_compression: if packed::compression_enabled(&self.objects())? {
                "auto"
            } else {
                "never"
            }
            .into(),
            storage,
            store_logical_bytes,
            store_allocated_bytes,
            revisions: revisions.len(),
            objects: objects.len(),
            object_bytes,
            object_storage_bytes,
            object_allocated_bytes: {
                #[cfg(unix)]
                {
                    Some(object_allocated_bytes)
                }
                #[cfg(not(unix))]
                {
                    None
                }
            },
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
        let revision = self.resolve_revision(&state, reference)?.1;
        let entry = revision
            .manifest
            .get(path)
            .ok_or_else(|| failure("path not found"))?;
        let EntryKind::File { size, chunks } = &entry.kind else {
            return Err(failure(
                "read requires a regular file; symbolic links are not followed",
            ));
        };
        let mut reader = packed::PointReader::open(&self.objects())?;
        let mut written = 0u64;
        for hash in chunks {
            let bytes = reader.read(hash)?;
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
        let manifest = self.resolve_revision(&state, reference)?.1.manifest;
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
        self.resolve_revision(&state, reference)?
            .1
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
            .resolve_revision(&state, reference)?
            .1
            .manifest
            .into_keys()
            .filter(|path| matcher.is_match(path))
            .collect())
    }

    /// Literal, UTF-8 text search in files up to the documented merge read bound.
    pub fn grep(&self, reference: &str, pattern: &str) -> Result<Vec<crate::GrepMatch>> {
        let _lock = self.lock()?;
        let state = self.state()?;
        let manifest = self.resolve_revision(&state, reference)?.1.manifest;
        let reader = packed::Reader::open(&self.objects())?;
        let mut matches = Vec::new();
        for (path, entry) in manifest {
            if matches!(entry.kind, EntryKind::File { size, .. } if size <= content::MAX_READ_FILE)
            {
                let bytes = content::read_file_from(&entry, &reader)?;
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

    pub fn backend(&self) -> Result<crate::BackendReport> {
        Ok(crate::BackendReport {
            native: copy_tree::native_backend_info(&self.root)?,
            projection: crate::ProjectionBackendInfo {
                compiled: cfg!(all(target_os = "linux", feature = "projection")),
                requires_root: true,
                fuse_device_present: Path::new("/dev/fuse").try_exists()?,
                note: "Explicit Linux FUSE/OverlayFS mounts only; device presence does not prove mount permission or kernel support. No helper is installed. External writers must be quiescent; this is not a sandbox.".into(),
            },
        })
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
            backend: workspace.backend.clone(),
            mount_state: self.mount_state(workspace),
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
            mount_error: None,
        }
    }

    fn lock(&self) -> Result<StoreLock> {
        let path = self.root.join("lock");
        require_file(&path)?;
        let file = platform::open_read_write(&path)?;
        file.try_lock()
            .map_err(|error| failure(&format!("store_locked: {error}")))?;
        Ok(StoreLock {
            _file: file,
            root: self.root.clone(),
            observations: RefCell::new(Meter::new(self.observation_budget)),
        })
    }

    fn lease(&self, workspace: &Workspace) -> Result<File> {
        let directory = self.root.join("native").join(&workspace.directory);
        require_directory(&directory)?;
        require_directory(&directory.join("tree"))?;
        let path = directory.join("lease");
        let file = platform::open_lease(&path)
            .map_err(|error| failure(&format!("workspace_busy: {error}")))?;
        file.try_lock().map_err(|error| {
            failure(&format!(
                "workspace_busy: active exec lease or lock error: {error}"
            ))
        })?;
        Ok(file)
    }

    fn state(&self) -> Result<State> {
        self.state_bytes().map(|(state, _)| state)
    }

    fn state_bytes(&self) -> Result<(State, Vec<u8>)> {
        require_file(&self.root.join("state.json"))?;
        let bytes = fs::read(self.root.join("state.json"))?;
        let state: State = serde_json::from_slice(&bytes)?;
        validate_state(&state)?;
        Ok((state, bytes))
    }

    fn save(&self, _lock: &StoreLock, state: &mut State) -> Result<()> {
        fault("save-before-backup")?;
        let (current, current_bytes) = self.state_bytes()?;
        if current.generation != state.generation {
            return Err(failure(
                "metadata generation changed; transaction not published",
            ));
        }
        let backup = self
            .root
            .join("backups")
            .join(format!("{}.json.zst", current.generation));
        // Preserve the exact validated published state, rather than allocating
        // and serializing the increasingly large workspace/trash map again.
        write_immutable(&backup, &metadata::encode(&current_bytes)?)?;
        fault("save-after-backup")?;
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
        fault("save-before-rename")?;
        platform::rename(&staged, &self.root.join("state.json"))?;
        fault("save-after-rename")?;
        // An error after rename has an uncertain publication outcome. Inspect
        // generation/head before retrying; never blindly repeat a mutation.
        sync_directory(&self.root)?;
        fault("save-after-root-sync")?;
        sync_directory(&self.root.join("staging"))?;
        fault("save-after-staging-sync")?;
        Ok(())
    }

    fn put_revision(&self, revision: &Revision) -> Result<String> {
        content::validate_manifest(&revision.manifest)?;
        for parent in &revision.parents {
            if !valid_hash(parent) {
                return Err(failure("invalid revision parent"));
            }
        }
        let stored = StoredRevision {
            parents: revision.parents.clone(),
            clock: revision.clock,
            message: revision.message.clone(),
            manifest: manifest_pages::write(&revision.manifest, &self.objects())?,
        };
        let bytes = serde_json::to_vec(&stored)?;
        let id = blake3::hash(&bytes).to_hex().to_string();
        write_immutable(
            &self.root.join("revisions").join(format!("{id}.json.zst")),
            &metadata::encode(&bytes)?,
        )?;
        Ok(id)
    }

    fn stored_revision(&self, id: &str) -> Result<StoredRevision> {
        if !valid_hash(id) {
            return Err(failure("invalid revision hash"));
        }
        let path = self.root.join("revisions").join(format!("{id}.json.zst"));
        require_file(&path)?;
        let (revision, hash): (StoredRevision, _) = metadata::read(&path)?;
        if hash.to_hex().as_str() != id {
            return Err(failure("revision hash mismatch"));
        }
        if revision.clock == 0 || revision.parents.iter().any(|parent| !valid_hash(parent)) {
            return Err(failure("invalid revision metadata"));
        }
        Ok(revision)
    }

    fn revision_with_reader(&self, id: &str, reader: &packed::Reader) -> Result<Revision> {
        let stored = self.stored_revision(id)?;
        let manifest = manifest_pages::read(&stored.manifest, reader)?;
        Ok(Revision {
            parents: stored.parents,
            clock: stored.clock,
            message: stored.message,
            manifest,
        })
    }

    fn revision(&self, id: &str) -> Result<Revision> {
        #[cfg(test)]
        lifecycle_tests::REVISION_READS.with(|count| count.set(count.get() + 1));
        let stored = self.stored_revision(id)?;
        let mut reader = packed::PointReader::open(&self.objects())?;
        let manifest = manifest_pages::read_point(&stored.manifest, &mut reader)?;
        Ok(Revision {
            parents: stored.parents,
            clock: stored.clock,
            message: stored.message,
            manifest,
        })
    }

    fn resolve(&self, state: &State, reference: &str) -> Result<String> {
        self.resolve_revision(state, reference).map(|(id, _)| id)
    }

    // An operation-local value, never a cross-operation or timestamp cache.
    // Callers that need the manifest retain this verified decode instead of
    // immediately reopening the same immutable revision.
    fn resolve_revision(&self, state: &State, reference: &str) -> Result<(String, Revision)> {
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
        let data = self.revision(&id)?;
        Ok((id, data))
    }

    fn new_native(&self) -> Result<String> {
        let id = new_id()?;
        let path = self.root.join("native").join(&id);
        platform::create_dir(&path, 0o700)?;
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
        let reader = packed::Reader::open(&self.objects())?;
        while let Some(id) = queue.pop_front() {
            if seen.insert(id.clone()) {
                queue.extend(self.revision_with_reader(&id, &reader)?.parents);
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
        self.protected_roots_inner(state, true)
    }

    fn protected_roots_inner(&self, state: &State, include_staging: bool) -> Result<Vec<String>> {
        let mut states = self.historical_states()?;
        if include_staging {
            states.extend(self.staged_observation_states()?);
        }
        states.push(state.clone());
        let mut roots = BTreeSet::new();
        for state in states {
            roots.extend(state.branches.into_values());
            for workspace in state.workspaces.into_values() {
                if let WorkspaceBackend::Projection { revision }
                | WorkspaceBackend::Writable { revision } = workspace.backend
                {
                    roots.insert(revision);
                }
                roots.insert(workspace.base_revision);
                if let Some(pending) = workspace.pending {
                    roots.insert(pending.candidate_revision);
                }
            }
            for item in state.trash.into_values() {
                roots.insert(item.revision);
                if let Some(workspace) = item.workspace {
                    if let WorkspaceBackend::Projection { revision }
                    | WorkspaceBackend::Writable { revision } = workspace.backend
                    {
                        roots.insert(revision);
                    }
                    roots.insert(workspace.base_revision);
                    if let Some(pending) = workspace.pending {
                        roots.insert(pending.candidate_revision);
                    }
                }
            }
        }
        // Immutable generation pins also cover a mount prepared before a
        // metadata publication that failed or whose outcome is unknown.
        for name in names(&self.root.join("native"))? {
            for pin_name in ["projection-base", "writable-base"] {
                let pin = self.root.join("native").join(&name).join(pin_name);
                match platform::symlink_metadata(&pin) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                    Ok(_) => require_file(&pin)?,
                }
                let revision = fs::read_to_string(pin)?;
                if !valid_hash(&revision) {
                    return Err(failure("invalid retained projection pin"));
                }
                roots.insert(revision);
            }
        }
        Ok(roots.into_iter().collect())
    }

    fn protected_directories(&self, state: &State) -> Result<BTreeSet<String>> {
        self.protected_directories_inner(state, true)
    }

    fn protected_directories_inner(
        &self,
        state: &State,
        include_staging: bool,
    ) -> Result<BTreeSet<String>> {
        let mut states = self.historical_states()?;
        if include_staging {
            states.extend(self.staged_observation_states()?);
        }
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
    observations::validate_descriptor(&workspace.observed)?;
    if workspace
        .observation_epoch
        .as_ref()
        .is_some_and(|epoch| !valid_hash(epoch))
    {
        return Err(failure("invalid observation session"));
    }
    if let WorkspaceBackend::Projection { revision } | WorkspaceBackend::Writable { revision } =
        &workspace.backend
        && !valid_hash(revision)
    {
        return Err(failure("invalid projection revision"));
    }
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
    if !platform::symlink_metadata(&path)?.is_dir() {
        return Err(failure(&format!(
            "not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_file(path: &Path) -> Result<()> {
    if !platform::symlink_metadata(path)?.is_file() {
        return Err(failure(&format!("not a regular file: {}", path.display())));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    Ok(platform::sync_directory(path)?)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = platform::create_new(path, 0o600)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    match platform::symlink_metadata(path) {
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
            platform::rename(&temporary, path)?;
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
    let mut observed = Observed::new();
    for (path, entry) in manifest {
        let metadata = platform::symlink_metadata(tree.join(path))?;
        observed.insert(
            path.clone(),
            content::Observation {
                id: entry.id.clone(),
                link_group: entry.link_group.clone(),
                device: metadata.dev(),
                inode: platform::inode(&metadata),
            },
        );
    }
    Ok(observed)
}

fn mutation_path(tree: &Path, relative: &str, allow_missing: bool) -> Result<PathBuf> {
    platform::validate_relative_path(relative)?;
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
            match platform::symlink_metadata(&current) {
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
            if let Ok(metadata) = platform::symlink_metadata(&target)
                && !metadata.is_file()
            {
                return Err(failure("write refuses symlinks and non-files"));
            }
            // This is already a private generation, so replacing a regular leaf
            // cannot expose partial data through a published workspace.
            let mut output = match platform::open_read_write(&target) {
                Ok(file) => {
                    file.set_len(0)?;
                    file
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    platform::create_new(&target, 0o644)?
                }
                Err(error) => return Err(error.into()),
            };
            output.write_all(content)?;
            output.sync_all()?;
        }
        Mutation::Mkdir { path } => {
            fs::create_dir(mutation_path(tree, path, true)?)?;
        }
        Mutation::Remove { path } => {
            let target = mutation_path(tree, path, false)?;
            if platform::symlink_metadata(&target)?.is_dir() {
                fs::remove_dir_all(target)?;
            } else {
                fs::remove_file(target)?;
            }
            observed.retain(|key, _| key != path && !key.starts_with(&format!("{path}/")));
        }
        Mutation::Move { from, to } => {
            let source = mutation_path(tree, from, false)?;
            let target = mutation_path(tree, to, true)?;
            if platform::symlink_metadata(&target).is_ok() {
                return Err(failure("move target already exists"));
            }
            if to.starts_with(&format!("{from}/")) {
                return Err(failure("cannot move a directory inside itself"));
            }
            platform::rename(&source, &target)?;
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
            let mut file = platform::open_read_write(&target)?;
            file.set_len(0)?;
            file.write_all(replacement.as_bytes())?;
            file.sync_all()?;
        }
        Mutation::Link { from, to } => {
            let source = mutation_path(tree, from, false)?;
            let target = mutation_path(tree, to, true)?;
            require_file(&source)?;
            platform::hard_link(&source, &target)?;
        }
        Mutation::Copy { from, to } => {
            let source = mutation_path(tree, from, false)?;
            let target = mutation_path(tree, to, true)?;
            if platform::symlink_metadata(&target).is_ok() {
                return Err(failure("copy target already exists"));
            }
            if to.starts_with(&format!("{from}/")) {
                return Err(failure("cannot copy a directory inside itself"));
            }
            copy_subtree(&source, &target, &mut BTreeMap::new())?;
        }
        Mutation::Chmod { path, mode } => {
            if cfg!(windows) {
                return Err(failure("POSIX chmod is unsupported on Windows"));
            }
            let target = mutation_path(tree, path, false)?;
            if platform::symlink_metadata(&target)?.is_symlink() {
                return Err(failure("chmod refuses symlinks"));
            }
            if *mode & !0o777 != 0 {
                return Err(failure("only ordinary permission bits are supported"));
            }
            platform::set_path_mode(&target, *mode)?;
        }
    }
    Ok(())
}

fn copy_subtree(
    source: &Path,
    target: &Path,
    links: &mut BTreeMap<(u64, u128), PathBuf>,
) -> Result<()> {
    let metadata = platform::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        #[cfg(windows)]
        return Err(failure("Windows reparse points are unsupported"));
        #[cfg(target_os = "macos")]
        copy_tree::set_symlink_permissions(target, metadata.mode() & 0o777)?;
        #[cfg(unix)]
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
        platform::set_path_mode(target, metadata.mode() & 0o777)?;
        platform::open_directory(target)?
            .set_times(fs::FileTimes::new().set_modified(metadata.modified()?))?;
        sync_directory(target)?;
    } else if metadata.is_file() {
        let identity = (metadata.dev(), platform::inode(&metadata));
        if let Some(previous) = links.get(&identity) {
            platform::hard_link(previous, target)?;
        } else {
            fs::copy(source, target)?;
            platform::set_path_mode(target, metadata.mode() & 0o777)?;
            #[cfg(unix)]
            let file = platform::open_regular(target)?;
            #[cfg(windows)]
            let file = platform::open_read_write(target)?;
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
    let mut file = platform::open_regular(path)?;
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
        let metadata = platform::symlink_metadata(entry.path())?;
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
    platform::validate_relative_path(relative)?;
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
            match platform::symlink_metadata(&current) {
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

type SyncedInodes = BTreeMap<(u64, u128), (u64, i64, i64, i64, i64)>;

fn sync_inode_once(
    path: &Path,
    metadata: &platform::Metadata,
    synced: &mut SyncedInodes,
) -> Result<()> {
    let key = (metadata.dev(), platform::inode(metadata));
    let fingerprint = (
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    );
    if let Some(previous) = synced.get(&key) {
        if *previous != fingerprint {
            return Err(failure("linked file changed during synchronization"));
        }
        return Ok(());
    }
    sync_verified_file(path, metadata)?;
    synced.insert(key, fingerprint);
    Ok(())
}

// Flush and validate one file without allocating an inode-deduplication map.
fn sync_verified_file(path: &Path, metadata: &platform::Metadata) -> Result<()> {
    let key = (metadata.dev(), platform::inode(metadata));
    let fingerprint = (
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    );
    platform::sync_file(path)?;
    let after = platform::symlink_metadata(path)?;
    if !after.is_file()
        || (after.dev(), platform::inode(&after)) != key
        || (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) != fingerprint
    {
        return Err(failure("file changed during synchronization"));
    }
    Ok(())
}

#[cfg(unix)]
mod native_sync;

fn sync_native_tree(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        native_sync::run(path)
    }
    #[cfg(not(unix))]
    {
        sync_native_entries(path, &mut BTreeMap::new())
    }
}

#[cfg(any(not(unix), test))]
fn sync_native_entries(path: &Path, synced: &mut SyncedInodes) -> Result<()> {
    let metadata = platform::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_native_entries(&entry?.path(), synced)?;
        }
        sync_directory(path)?;
    } else if metadata.is_file() {
        sync_inode_once(path, &metadata, synced)?;
    } else if !metadata.file_type().is_symlink() {
        return Err(failure(
            "special file encountered while syncing native generation",
        ));
    }
    Ok(())
}

/// Overlay whiteouts are namespace metadata, never device contents to open.
#[cfg(windows)]
fn sync_overlay_upper(_path: &Path) -> Result<()> {
    Err(failure("projection requires Linux"))
}

#[cfg(unix)]
fn sync_overlay_upper(path: &Path) -> Result<()> {
    sync_overlay_entries(path, &mut BTreeMap::new())
}

#[cfg(unix)]
fn sync_overlay_entries(path: &Path, synced: &mut SyncedInodes) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = platform::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_overlay_entries(&entry?.path(), synced)?;
        }
        sync_directory(path)?;
    } else if metadata.is_file() {
        sync_inode_once(path, &metadata, synced)?;
    } else if !(metadata.file_type().is_symlink()
        || metadata.file_type().is_char_device() && metadata.rdev() == 0)
    {
        return Err(failure("unsupported special file in projection upper"));
    }
    Ok(())
}

// Catalog unique logical chunks; never interpret pack/index names as hashes.
fn object_catalog(path: &Path, reader: &packed::Reader) -> Result<BTreeMap<String, u64>> {
    let mut catalog = reader.object_sizes();
    for name in names(path)? {
        if valid_hash(&name) {
            let file = platform::open_regular(&path.join(&name))?;
            catalog.entry(name).or_insert(file.metadata()?.len());
        }
    }
    Ok(catalog)
}

#[inline]
fn fault(point: &'static str) -> Result<()> {
    #[cfg(test)]
    observation_tests::inject(point)?;
    #[cfg(not(test))]
    let _ = point;
    Ok(())
}
