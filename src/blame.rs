//! Line attribution follows stable file identity through first-parent history.
//!
//! Renames and metadata-only changes preserve origins. At a merge, lines equal
//! to the first parent keep their earlier origins; other lines are attributed to
//! the merge revision, not searched for in a second parent. Diffy's line diff
//! chooses matching sequences; this does not infer copied or moved-line origins.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::content::{self, Entry, EntryKind};
use crate::{Result, Revision};

const MAX_LINES: usize = 100_000;
const MAX_REVISIONS: usize = 10_000;

#[derive(Clone, Debug, Serialize)]
pub struct BlameLine {
    /// One-based line number in the requested revision.
    pub line: usize,
    /// Original text, including its newline when present.
    pub text: String,
    pub revision: String,
    /// One-based line number in the revision that introduced this line.
    pub original_line: usize,
    pub original_path: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct LineBlame {
    pub path: String,
    pub identity: String,
    pub revision: String,
    pub granularity: String,
    pub lines: Vec<BlameLine>,
}

/// Attribute every current line to its introduction on the first-parent chain.
///
/// The loader must verify revision addresses. The requested entry must be a
/// regular UTF-8 text file. Every compared historical file must also be UTF-8
/// without NUL and fit the 16 MiB / 100,000-line limits. Binary or oversized
/// historical text is an error, not an inferred origin boundary. History is
/// limited to 10,000 revisions; any limit returns an error, never partial blame.
pub fn attribute(
    revision_id: &str,
    path: &str,
    objects: &Path,
    mut load_revision: impl FnMut(&str) -> Result<Revision>,
) -> Result<LineBlame> {
    let mut current_id = revision_id.to_owned();
    let mut current = load_revision(&current_id)?;
    content::validate_manifest(&current.manifest)?;
    let mut entry = current
        .manifest
        .get(path)
        .ok_or("blame path not found in revision")?
        .clone();
    let identity = entry.id.clone();
    let mut current_path = path.to_owned();
    let mut text = read_text(&entry, objects)?;
    let mut lines: Vec<_> = text
        .split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| BlameLine {
            line: i + 1,
            text: line.to_owned(),
            revision: String::new(),
            original_line: 0,
            original_path: String::new(),
            message: String::new(),
        })
        .collect();
    // Each pending pair maps an output line to its position in current text.
    let mut pending: Vec<_> = (0..lines.len()).map(|i| (i, i)).collect();
    let mut seen = BTreeSet::from([current_id.clone()]);
    while !pending.is_empty() {
        let Some(parent_id) = current.parents.first().cloned() else {
            break;
        };
        if seen.len() >= MAX_REVISIONS {
            return Err("blame history exceeds 10000 first-parent revisions".into());
        }
        if !seen.insert(parent_id.clone()) {
            return Err("blame encountered a revision cycle".into());
        }
        let parent = load_revision(&parent_id)?;
        content::validate_manifest(&parent.manifest)?;
        let Some((parent_path, parent_entry)) = parent
            .manifest
            .iter()
            .find(|(_, candidate)| candidate.id == identity)
        else {
            break;
        };
        if !matches!(parent_entry.kind, EntryKind::File { .. }) {
            break;
        }
        let parent_path = parent_path.clone();
        let parent_entry = parent_entry.clone();
        if parent_entry.kind != entry.kind {
            let parent_text = read_text(&parent_entry, objects)?;
            let mapping = unchanged_lines(&parent_text, &text)?;
            let mut inherited = Vec::with_capacity(pending.len());
            for (output_line, current_line) in pending {
                match mapping
                    .get(current_line)
                    .ok_or("invalid blame line mapping")?
                {
                    Some(parent_line) => inherited.push((output_line, *parent_line)),
                    None => set_origin(
                        &mut lines[output_line],
                        current_line,
                        &current_id,
                        &current_path,
                        &current.message,
                    ),
                }
            }
            pending = inherited;
            text = parent_text;
        }
        current_id = parent_id;
        current_path = parent_path;
        entry = parent_entry;
        current = parent;
    }
    for (output_line, current_line) in pending {
        set_origin(
            &mut lines[output_line],
            current_line,
            &current_id,
            &current_path,
            &current.message,
        );
    }
    Ok(LineBlame {
        path: path.to_owned(),
        identity,
        revision: revision_id.to_owned(),
        granularity: "line; first-parent history; stable-identity renames".to_owned(),
        lines,
    })
}

fn set_origin(
    line: &mut BlameLine,
    original_line: usize,
    revision: &str,
    path: &str,
    message: &str,
) {
    line.original_line = original_line + 1;
    line.revision = revision.to_owned();
    line.original_path = path.to_owned();
    line.message = message.to_owned();
}

fn read_text(entry: &Entry, objects: &Path) -> Result<String> {
    let EntryKind::File { size, .. } = &entry.kind else {
        return Err("line blame requires a regular text file".into());
    };
    if *size > content::MAX_READ_FILE {
        return Err("line blame rejects files larger than 16 MiB".into());
    }
    let bytes = content::read_file(entry, objects)?;
    if bytes.contains(&0) {
        return Err("line blame rejects binary content containing NUL".into());
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| "line blame requires UTF-8 text, not binary content")?;
    if text.split_inclusive('\n').take(MAX_LINES + 1).count() > MAX_LINES {
        return Err("line blame exceeds 100000 lines".into());
    }
    Ok(text)
}

fn unchanged_lines(parent: &str, current: &str) -> Result<Vec<Option<usize>>> {
    let current_count = current.split_inclusive('\n').count();
    if parent == current {
        return Ok((0..current_count).map(Some).collect());
    }
    // Full context exposes every unchanged line in the mature diff engine's
    // edit script, avoiding custom LCS or parsing formatted unified patches.
    let mut options = diffy::DiffOptions::new();
    options.set_context_len(MAX_LINES);
    let patch = options.create_patch(parent, current);
    let mut mapping = Vec::with_capacity(current_count);
    let mut parent_line = 0;
    for hunk in patch.hunks() {
        for line in hunk.lines() {
            match line {
                diffy::Line::Context(_) => {
                    mapping.push(Some(parent_line));
                    parent_line += 1;
                }
                diffy::Line::Delete(_) => parent_line += 1,
                diffy::Line::Insert(_) => mapping.push(None),
            }
        }
    }
    if mapping.len() != current_count || parent_line != parent.split_inclusive('\n').count() {
        return Err("diff engine returned an incomplete blame mapping".into());
    }
    Ok(mapping)
}
