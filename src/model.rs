use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::content::{Manifest, Observed};
use crate::merge::MergeConflict;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Revision {
    pub parents: Vec<String>,
    pub clock: u64,
    pub message: String,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub branch: String,
    pub base_revision: String,
    pub directory: String,
    pub observed: Observed,
    #[serde(default)]
    pub pending: Option<PendingMerge>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrashItem {
    pub token: String,
    pub name: String,
    pub revision: String,
    pub workspace: Option<Workspace>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct State {
    pub version: u32,
    pub generation: u64,
    pub branches: BTreeMap<String, String>,
    pub workspaces: BTreeMap<String, Workspace>,
    pub trash: BTreeMap<String, TrashItem>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceResult {
    pub name: String,
    pub branch: String,
    pub base_revision: String,
    pub path: PathBuf,
    pub pending_conflicts: Vec<MergeConflict>,
    pub copy_stats: Option<crate::copy_tree::CopyStats>,
}

#[derive(Debug, Serialize)]
pub struct CommitResult {
    pub committed: bool,
    pub revision: String,
    pub branch: String,
    pub previous_head: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BranchResult {
    pub name: String,
    pub revision: String,
}

#[derive(Debug, Serialize)]
pub struct MergeOutcome {
    pub committed: bool,
    pub revision: Option<String>,
    pub branch: String,
    pub merge_base: String,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Debug, Serialize)]
pub struct RevisionSummary {
    pub revision: String,
    pub parents: Vec<String>,
    pub clock: u64,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Change {
    pub path: String,
    pub previous_path: Option<String>,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    pub branches: BTreeMap<String, String>,
    pub workspaces: BTreeMap<String, WorkspaceResult>,
    pub trash: Vec<TrashItem>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub generation: u64,
    pub store: PathBuf,
    pub listing: Listing,
    pub stale_workspaces: Vec<String>,
    pub active_leases: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub errors: Vec<String>,
    pub orphan_revisions: Vec<String>,
    pub orphan_workspaces: Vec<String>,
    pub staging_entries: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GcPlan {
    pub dry_run: bool,
    pub generation: u64,
    pub unreachable_objects: Vec<String>,
    pub unreachable_revisions: Vec<String>,
    pub orphan_workspaces: Vec<String>,
    pub reclaimable_bytes: u64,
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub revisions: usize,
    pub objects: usize,
    pub object_bytes: u64,
    pub reachable_logical_bytes: u64,
    pub workspaces: usize,
    pub branches: usize,
    pub trash_entries: usize,
}

/// A transaction edits a private generation, then publishes through the same
/// branch-head CAS used by checkpoint. No operation edits an existing generation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Mutation {
    Write {
        path: String,
        content: Vec<u8>,
    },
    Mkdir {
        path: String,
    },
    Remove {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    Chmod {
        path: String,
        mode: u32,
    },
    Edit {
        path: String,
        old: String,
        new: String,
    },
    Copy {
        from: String,
        to: String,
    },
}

#[derive(Debug, Serialize)]
pub struct PathEntry {
    pub path: String,
    pub entry: crate::content::Entry,
}

#[derive(Debug, Serialize)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingMerge {
    pub candidate_revision: String,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Debug, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub previous_path: Option<String>,
    pub kind: String,
    pub patch: Option<String>,
    pub binary: bool,
}
