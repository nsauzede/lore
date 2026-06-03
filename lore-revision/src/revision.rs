// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod amend;
pub mod bisect;
pub mod cherry_pick;
pub mod diff;
pub mod history;
pub mod info;
pub mod restore;
pub mod revert;
pub mod sync;

use std::str::FromStr;
use std::sync::Arc;

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use lore_transport::RevisionListIdentifier;
use serde::Deserialize;
use serde::Serialize;
use tokio::join;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::branch;
use crate::change;
use crate::change::FileAction;
use crate::change::NodeChange;
use crate::change::is_conflict;
use crate::errors::RevisionNotFound;
use crate::event;
use crate::filter::Filter;
use crate::filter::FilterMode;
use crate::find;
use crate::history::find_branch_point;
use crate::interface::LoreString;
use crate::lore::*;
use crate::lore_debug;
use crate::lore_info;
use crate::lore_trace;
use crate::lore_warn;
use crate::metadata;
use crate::metadata::Metadata;
use crate::node::NodeFileMetadata;
use crate::node::NodeFileMetadataBlock;
use crate::node::NodeIDExt;
use crate::repository::RepositoryContext;
use crate::state;
use crate::state::State;
use crate::state::StateError;
use crate::state::TreePath;
use crate::state::gather_tree_paths;
use crate::util::path::RelativePath;

/// Base metadata for revision
#[derive(Default)]
pub struct RevisionMetadata {
    /// Commit message
    pub message: String,
    /// Timestamp, number of milliseconds since Unix epoch in UTC
    pub timestamp: u64,
    /// Branch where revision was committed
    pub branch: BranchId,
    /// Created by user names or identifiers (original authors of the revision)
    pub created_by: Option<String>,
    /// Committed by user name or identifier (user committing the revision to the branch)
    pub committed_by: Option<String>,
    /// Reviewed by user names or identifiers (user performing change review and approving revision)
    pub reviewed_by: Option<String>,
    /// Merged by user name or identifier (user performing the merge)
    pub merged_by: Option<String>,
    /// Perforce changelist association
    pub p4_changelist: Option<String>,
    /// Revision this was cherry-picked from
    pub cherry_picked_from: Hash,
    /// Revision this was reverted from
    pub reverted_from: Hash,
    /// Change Request ID
    pub change_request: Option<String>,
}

impl RevisionMetadata {
    pub fn from_metadata(metadata: Metadata) -> Self {
        let mut revision_metadata = RevisionMetadata::default();
        let _ = metadata.walk(|key, value, _value_type| {
            let key = std::str::from_utf8(key).unwrap_or("<binary>");
            match key {
                metadata::MESSAGE => {
                    revision_metadata.message =
                        std::str::from_utf8(value).unwrap_or("<binary>").to_string();
                }
                metadata::TIMESTAMP if value.len() == std::mem::size_of::<u64>() => {
                    revision_metadata.timestamp = u64::from_le_bytes(value.try_into().unwrap());
                }
                metadata::BRANCH if value.len() == std::mem::size_of::<Context>() => {
                    revision_metadata.branch = value.into();
                }
                metadata::CREATED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.created_by = Some(value.to_string());
                    }
                }
                metadata::COMMITTED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.committed_by = Some(value.to_string());
                    }
                }
                metadata::REVIEWED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.reviewed_by = Some(value.to_string());
                    }
                }
                metadata::MERGED_BY => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.merged_by = Some(value.to_string());
                    }
                }
                metadata::P4_CHANGELIST => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.p4_changelist = Some(value.to_string());
                    }
                }
                metadata::CHERRY_PICKED_FROM if value.len() == std::mem::size_of::<Hash>() => {
                    revision_metadata.cherry_picked_from = value.into();
                }
                metadata::REVERTED_FROM if value.len() == std::mem::size_of::<Hash>() => {
                    revision_metadata.reverted_from = value.into();
                }
                metadata::CHANGE_REQUEST => {
                    if let Ok(value) = std::str::from_utf8(value) {
                        revision_metadata.change_request = Some(value.to_string());
                    }
                }
                _ => {}
            }
        });
        revision_metadata
    }
}

#[derive(Debug, Default)]
pub struct DiffResult {
    /// Base revision
    pub base: Hash,
    /// Source revision
    pub source: Hash,
    /// Target revision
    pub target: Hash,
    /// Set of changes
    pub changes: Vec<NodeChange>,
    /// Set of conflicts, first element is source change, second element is target change
    pub conflicts: Vec<(NodeChange, NodeChange)>,
}

/// One item emitted on the sender by a streaming 3-way diff: a single per-path
/// `Change`, or a `Conflict` pair (source-side change, target-side change).
/// Mirrors the `oneof payload` shape of `lore.thin_client.v1.RevisionDiffResponse`.
///
/// `Conflict` boxes its pair so that `Change` (the hot path) does not pay
/// the inline-pair padding cost on every send. `Change` itself stays
/// unboxed because it is the common case and unboxed flow is cheaper than
/// the heap allocation a `Box<NodeChange>` would add per item.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum DiffItem {
    Change(NodeChange),
    Conflict(Box<(NodeChange, NodeChange)>),
}

/// The small fixed-size facts a streaming 3-way diff reports after the stream
/// completes. Replaces the non-streaming fields of `DiffResult` (everything
/// except the `changes` / `conflicts` vectors, which are streamed instead).
#[derive(Debug, Default, Clone, Copy)]
pub struct Diff3Summary {
    /// Resolved common-ancestor base revision.
    pub base: Hash,
    /// Source revision (from-side).
    pub source: Hash,
    /// Target revision (to-side).
    pub target: Hash,
}

#[allow(clippy::doc_overindented_list_items)]
/// Streaming wrapper around `diff3_collect`. Computes the 3-way diff
/// internally (the algorithm requires both intermediate sets present for
/// its sort-merge-join and conflict-resolution passes), then emits each
/// resulting change and conflict as a `DiffItem` on `tx`. Returns a
/// `Diff3Summary` carrying the base / source / target revisions.
///
/// Memory note: 3-way diff intrinsically buffers both base→source and
/// base→target intermediate sets during merge — this wrapper provides the
/// streaming surface for delivery but does not reduce the internal
/// buffering. See LEP `2026-05-12-streaming-revision-diff-engine.md`
/// Assumption #2 for the design rationale.
pub async fn diff3(
    repository: Arc<RepositoryContext>,
    base: Hash,
    source: Hash,
    target: Hash,
    path: Option<RelativePath>,
    include_same: bool,
    tx: mpsc::Sender<Result<DiffItem, StateError>>,
) -> Result<Diff3Summary, StateError> {
    let diff = diff3_collect(repository, base, source, target, path, include_same).await?;
    let summary = Diff3Summary {
        base: diff.base,
        source: diff.source,
        target: diff.target,
    };
    for change in diff.changes {
        tx.send(Ok(DiffItem::Change(change)))
            .await
            .map_err(|_send_err| StateError::internal("3-way diff receiver dropped"))?;
    }
    for (source_change, target_change) in diff.conflicts {
        tx.send(Ok(DiffItem::Conflict(Box::new((
            source_change,
            target_change,
        )))))
        .await
        .map_err(|_send_err| StateError::internal("3-way diff receiver dropped"))?;
    }
    Ok(summary)
}

#[allow(clippy::doc_overindented_list_items)]
/// Calculate the difference between two revisions with a common base ancestor,
/// as the set of changes that describe applying the delta between source and base
/// on top of target. This works by calculating
///  - `diff(base, source)`: The set of changes describing delta from base to source, `ds`
///  - `diff(base, target)`: The set of changes describing delta from base to target, `dt`
///  - `ds - dt`: The set difference between ds and dt, i.e the set of changes that are
///               in ds but not in dt. That is, the set of changes performed between
///               base and source that has not been performed between base and target.
///               Any identical operation in both sets is thus ignored. This also takes
///               merges into account by walking file histories.
///
/// Result is the unique set of changes that occurred between base and source, and a
/// set of conflicting changes.
pub async fn diff3_collect(
    repository: Arc<RepositoryContext>,
    base: Hash,
    source: Hash,
    target: Hash,
    path: Option<RelativePath>,
    include_same: bool,
) -> Result<DiffResult, StateError> {
    let (state_base, state_source, state_target) = join!(
        State::deserialize(repository.clone(), base),
        State::deserialize(repository.clone(), source),
        State::deserialize(repository.clone(), target)
    );
    let (state_base, state_source, state_target) = (state_base?, state_source?, state_target?);

    let source_branch = state_source
        .revision_metadata(repository.clone())
        .await?
        .branch;

    let target_branch = state_target
        .revision_metadata(repository.clone())
        .await?
        .branch;

    let base_revision_number = state_base.revision_number();
    let source_revision_number = state_source.revision_number();
    let target_revision_number = state_target.revision_number();
    lore_info!(
        "Calculating diff between\n  base {} -> {}\n  source {} -> {}\n  target {} -> {}",
        base_revision_number,
        state_base.revision(),
        source_revision_number,
        state_source.revision(),
        target_revision_number,
        state_target.revision()
    );

    // Find the changes on source branch first in order to use this as a filter to target branch.
    // This improves performance when diffing a feature branch with low churn onto a main branch
    // with high churn, by reducing the number of considered changes by an order of magnitude
    lore_info!("Diff source branch revisions");
    let mut source_changes = state::diff_collect(
        repository.clone(),
        state_base.clone(),
        repository.clone(),
        state_source.clone(),
        path.clone(),
        FilterMode::View,
    )
    .await?;

    // Ignore source changes that just indicate a change in file ID
    source_changes.retain(|change| {
        change.from.address.hash.is_zero()
            || change.action == FileAction::Move
            || change.from.address.hash != change.to.address.hash
    });

    lore_info!("Sorting {} source changes", source_changes.len());
    change::sort_by_path(&mut source_changes);

    let mut filter_from_source = false;
    let target_filter = if source_changes.len() < 10000 {
        filter_from_source = true;
        let mut target_filter = Filter::default();
        if let Err(err) = target_filter.view.add_exclusion("**") {
            filter_from_source = false;
            lore_warn!("Failed to add target filter global exclusion: {err}");
        } else {
            for change in source_changes.iter() {
                if let Err(err) = target_filter.view.add_inclusion(change.path.as_str()) {
                    filter_from_source = false;
                    lore_warn!("Failed to add target filter re-inclusion: {err}");
                    break;
                }
                if let Some(from_path) = change.from_path.as_ref()
                    && let Err(err) = target_filter.view.add_inclusion(from_path.as_str())
                {
                    filter_from_source = false;
                    lore_warn!("Failed to add target filter re-inclusion of from path: {err}");
                    break;
                }
            }
        }
        if filter_from_source {
            Arc::new(target_filter)
        } else {
            repository.filter.clone()
        }
    } else {
        repository.filter.clone()
    };

    let repository = Arc::new(repository.to_filter_context(target_filter));

    lore_info!("Diff target branch revisions");
    let mut target_changes = state::diff_collect(
        repository.clone(),
        state_base.clone(),
        repository.clone(),
        state_target.clone(),
        path.clone(),
        FilterMode::View,
    )
    .await?;

    // Ignore target changes that just indicate a change in file ID
    target_changes.retain(|change| {
        change.from.address.hash.is_zero() || change.from.address.hash != change.to.address.hash
    });

    if filter_from_source {
        lore_info!("Sorting {} filtered target changes", target_changes.len());
    } else {
        lore_info!("Sorting {} target changes", target_changes.len());
    }
    change::sort_by_path(&mut target_changes);

    if filter_from_source {
        lore_info!(
            "Found {} changes on source branch, resulting in {} filtered changes on target branch",
            source_changes.len(),
            target_changes.len()
        );
    } else {
        lore_info!(
            "Found {} changes on source branch, {} changes on target branch",
            source_changes.len(),
            target_changes.len()
        );
    }

    let mut diff = DiffResult {
        base,
        source,
        target,
        changes: vec![],
        conflicts: vec![],
    };

    // Merge the sorted changes
    let mut isource = 0;
    let mut itarget = 0;
    while isource < source_changes.len() && itarget < target_changes.len() {
        let source_change = &source_changes[isource];
        let target_change = &target_changes[itarget];
        let compare = target_change.path.as_str().cmp(source_change.path.as_str());
        match compare {
            std::cmp::Ordering::Equal => {
                if is_conflict(source_change, target_change, true).await? {
                    let mut source_change = source_change.clone();
                    let mut target_change = target_change.clone();
                    source_change.flags = change::Flags::Conflict;
                    target_change.flags = change::Flags::Conflict;
                    diff.conflicts.push((source_change, target_change));
                } else if include_same {
                    if diff.changes.is_empty()
                        || diff.changes[diff.changes.len() - 1].path != source_change.path
                    {
                        lore_trace!(
                            "Including identical source change {isource} {}",
                            source_change.path.as_str()
                        );
                        diff.changes.push(source_change.clone());
                    } else {
                        lore_trace!(
                            "Identical source change {isource} {} already included",
                            source_change.path.as_str()
                        );
                    }
                }
                itarget += 1;
                isource += 1;
            }
            std::cmp::Ordering::Less => {
                diff.changes.push(target_change.clone());
                itarget += 1;
            }
            std::cmp::Ordering::Greater => {
                diff.changes.push(source_change.clone());
                isource += 1;
            }
        }
    }

    while itarget < target_changes.len() {
        diff.changes.push(target_changes[itarget].clone());
        itarget += 1;
    }
    while isource < source_changes.len() {
        diff.changes.push(source_changes[isource].clone());
        isource += 1;
    }

    // Handle Move + other-change-at-from-path interactions:
    // When a source Move has from_path=X and another change exists at path X:
    // - If the other change is a Delete (divergent move): pair as conflict
    // - If the Move didn't change content (pure rename) and other is Keep/Modify:
    //   absorb into the Move (the rename preserves the modified content)
    // - If the Move also changed content and other is Keep/Modify:
    //   pair as conflict (both branches modified content differently)
    {
        let mut from_path_conflict_pairs: Vec<(usize, usize)> = Vec::new();
        let mut from_path_absorbed: Vec<usize> = Vec::new();
        for (outer_index, change_move) in diff.changes.iter().enumerate() {
            if change_move.action == FileAction::Move
                && let Some(ref from_path) = change_move.from_path
            {
                for (inner_index, change_match) in diff.changes.iter().enumerate() {
                    if outer_index != inner_index
                        && change_match.path.as_str() == from_path.as_str()
                    {
                        if change_match.action == FileAction::Delete {
                            // Divergent move: pair as conflict
                            from_path_conflict_pairs.push((outer_index, inner_index));
                        } else if change_move.from.address.hash == change_move.to.address.hash {
                            // Pure rename + modify on other branch: absorb the
                            // modify since the rename preserves the modified content
                            from_path_absorbed.push(inner_index);
                        } else {
                            // Move with content change + modify on other branch:
                            // both branches changed content, pair as conflict
                            from_path_conflict_pairs.push((outer_index, inner_index));
                        }
                    }
                }
            }
        }
        let mut remove = vec![false; diff.changes.len()];
        for &absorbed_idx in from_path_absorbed.iter() {
            remove[absorbed_idx] = true;
        }
        for &(move_idx, other_idx) in from_path_conflict_pairs.iter() {
            if !remove[move_idx] && !remove[other_idx] {
                let mut move_change = diff.changes[move_idx].clone();
                let mut other_change = diff.changes[other_idx].clone();
                move_change.flags = change::Flags::Conflict;
                other_change.flags = change::Flags::Conflict;
                diff.conflicts.push((move_change, other_change));
                remove[move_idx] = true;
                remove[other_idx] = true;
            }
        }
        let mut i = diff.changes.len();
        while i > 0 {
            i -= 1;
            if remove[i] {
                diff.changes.remove(i);
            }
        }
    }

    if !diff.conflicts.is_empty() {
        // If the changes list contains a deleted directory, remove it if there is a
        // conflict in the conflicts list that overlaps the directory path.
        // If there is no overlapping conflict, we keep it as it is a desired change.
        diff.changes.retain(|item| {
            if item.from.flags.is_directory() && item.action == FileAction::Delete {
                for (from, _to) in diff.conflicts.iter() {
                    if from.path.overlaps(&item.path) {
                        return false;
                    }
                }
            }
            true
        });
        lore_debug!("Identify resolved conflicts through merges");
    }

    // Iterate the conflicts and resolve against file history to
    // see if conflict is already resolved by a previous merge.
    // Cap the in-flight task count to bound peak memory — each task
    // walks file history and may pin several `Arc<State>` instances
    // (each with its own block cache) for the duration of the walk.
    const MAX_TASK_COUNT: usize = 1000;
    let mut merge_check_tasks = JoinSet::new();
    let mut merge_error: Result<(), StateError> = Ok(());
    let mut task_error: Result<(), StateError> = Ok(());
    let mut final_conflicts = vec![];
    for index in 0..diff.conflicts.len() {
        let conflict = &diff.conflicts[index];
        let state_base = state_base.clone();
        let source_conflict = conflict.0.clone();
        let target_conflict = conflict.1.clone();
        lore_spawn!(merge_check_tasks, async move {
            lore_debug!(
                "Check if changes to {} are merged from target branch {} into source branch {}",
                target_conflict.path.as_str(),
                target_branch,
                source_branch
            );
            let is_merged_from_target = is_last_change_merged(
                target_conflict.clone(),
                target_branch,
                source_conflict.clone(),
                source_branch,
                state_base.clone(),
            )
            .await?;

            lore_debug!(
                "Check if changes to {} are merged from source branch {} into target branch {} {}",
                source_conflict.path.as_str(),
                source_branch,
                target_branch,
                target_conflict.path.as_str()
            );
            let is_merged_from_source = is_last_change_merged(
                source_conflict,
                source_branch,
                target_conflict,
                target_branch,
                state_base,
            )
            .await?;

            Ok((index, is_merged_from_source, is_merged_from_target))
        });

        while merge_check_tasks.len() >= MAX_TASK_COUNT
            && let Some(result) = merge_check_tasks.join_next().await
        {
            apply_merge_check_result(
                result,
                &mut diff,
                &mut final_conflicts,
                &mut merge_error,
                &mut task_error,
            );
        }
    }

    while let Some(result) = merge_check_tasks.join_next().await {
        apply_merge_check_result(
            result,
            &mut diff,
            &mut final_conflicts,
            &mut merge_error,
            &mut task_error,
        );
    }
    merge_error?;
    task_error?;

    diff.conflicts = final_conflicts;
    if !diff.conflicts.is_empty() {
        lore_debug!("Final {} conflicts", diff.conflicts.len());
    }

    Ok(diff)
}

fn apply_merge_check_result(
    result: Result<Result<(usize, bool, bool), StateError>, tokio::task::JoinError>,
    diff: &mut DiffResult,
    final_conflicts: &mut Vec<(NodeChange, NodeChange)>,
    merge_error: &mut Result<(), StateError>,
    task_error: &mut Result<(), StateError>,
) {
    match result {
        Ok(Ok((index, is_merged_from_source, is_merged_from_target))) => {
            if !is_merged_from_source && !is_merged_from_target {
                lore_debug!("Conflict remains: {:?}", diff.conflicts[index]);
                final_conflicts.push(diff.conflicts[index].clone());
            } else if !is_merged_from_source {
                let mut change = diff.conflicts[index].0.clone();

                lore_debug!("Conflict resolved: {:?}", diff.conflicts[index]);
                lore_debug!("  source change remains: {:?}", change);

                // If the file was added on source branch and either added or
                // modified on target branch, show the change as modified
                if diff.conflicts[index].0.action == FileAction::Add
                    && diff.conflicts[index].1.action != FileAction::Delete
                {
                    change.action = FileAction::Keep;
                }

                // Since the target was merged into source, the change is actually
                // from the target state to the source current latest state
                change.from = diff.conflicts[index].1.to.clone();

                diff.changes.push(change);
            } else {
                lore_debug!(
                    "Conflict resolved, source change merged: {:?}",
                    diff.conflicts[index]
                );
            }
        }
        Ok(Err(err)) => {
            if merge_error.is_ok() {
                *merge_error = Err(err);
            }
        }
        Err(_) => {
            *task_error = Err(StateError::internal("Task failure"));
        }
    }
}

async fn is_last_change_merged(
    source_conflict: NodeChange,
    source_branch: BranchId,
    target_conflict: NodeChange,
    target_branch: BranchId,
    state_branch_point: Arc<State>,
) -> Result<bool, StateError> {
    lore_debug!(
        "{} find last modified source revision",
        source_conflict.path.as_str()
    );
    let (last_source_modified_revision, last_source_modified_revision_number) =
        find_last_modified_revision(&source_conflict, state_branch_point.clone()).await?;
    lore_debug!(
        "{} last modified source {} revision {} -> {}",
        source_conflict.path.as_str(),
        source_branch,
        last_source_modified_revision,
        last_source_modified_revision_number,
    );

    lore_debug!(
        "{} find last merged source {} -> target {} revision",
        source_conflict.path.as_str(),
        source_branch,
        target_branch,
    );
    if let Some((last_merged_from_source_revision, last_merged_from_source_revision_number)) =
        find_last_merged_revision(
            &target_conflict,
            source_branch,
            target_branch,
            state_branch_point.clone(),
        )
        .await?
    {
        lore_debug!(
            "{} last merged from source {} into target {} revision {} -> {}",
            source_conflict.path.as_str(),
            source_branch,
            target_branch,
            last_merged_from_source_revision,
            last_merged_from_source_revision_number,
        );

        if last_source_modified_revision_number <= last_merged_from_source_revision_number {
            lore_debug!(
                "Final change check for {} is MERGED - last source merged revision {}, last source modified revision {}",
                source_conflict.path.as_str(),
                last_merged_from_source_revision_number,
                last_source_modified_revision_number,
            );
            Ok(true)
        } else {
            lore_debug!(
                "Final change merge check for {} is UNMERGED - last source merged revision {}, last source modified revision {}",
                source_conflict.path.as_str(),
                last_merged_from_source_revision_number,
                last_source_modified_revision_number,
            );
            Ok(false)
        }
    } else {
        lore_debug!(
            "Final change merge check for {} is UNMERGED - no merged revision found",
            source_conflict.path.as_str()
        );
        Ok(false)
    }
}

async fn find_last_modified_revision(
    change: &NodeChange,
    state_branch_point: Arc<State>,
) -> Result<(Hash, u64), StateError> {
    if !change.to.node.is_valid_node_id() {
        // File was deleted from the target state, need to walk history to find it
        // TODO(mjansson): Figure out a better way to store this metadata around file deletions
        lore_debug!("Find last modified revision without to state, iterate revisions");

        let repository = change.from.repository.clone();
        let mut state_current = change.to.state.clone();
        let mut state_parent = state_current.clone();
        while state_current.revision_number() >= state_branch_point.revision_number() {
            if let Ok(node_link) = state_current
                .find_node_link(repository.clone(), change.path.as_str())
                .await
            {
                // If the node was existing in the current state, it means it was deleted in the previous (parent) revision
                lore_debug!(
                    "Found {} existing in revision {} - {}",
                    change.path.as_str(),
                    state_current.revision(),
                    state_current.revision_number()
                );
                if node_link.is_valid() {
                    lore_debug!(
                        "Last modified {} in parent revision {} - {}",
                        change.path.as_str(),
                        state_parent.revision(),
                        state_parent.revision_number()
                    );
                    return Ok((state_parent.revision(), state_parent.revision_number()));
                }
            }
            state_parent = state_current.clone();
            state_current =
                State::deserialize(repository.clone(), state_current.parent_self()).await?;
        }

        lore_debug!(
            "Did not find node {} last modified, using branch point revision {} - {}",
            change.path.as_str(),
            state_branch_point.revision(),
            state_branch_point.revision_number()
        );

        return Ok((
            state_branch_point.revision(),
            state_branch_point.revision_number(),
        ));
    };

    let repository = change.to.repository.clone();
    let state = change.to.state.clone();
    let node_id = change.to.node;

    let mut last_modified_revision = state.revision();
    let mut last_modified_revision_number = state.revision_number();

    // Check if node was modified in the LATEST revision
    if let Ok(Some(_node_delta)) = state.node_delta(repository.clone(), node_id).await {
        lore_debug!(
            "{} found last modified revision {} -> {} (HEAD)",
            change.path.as_str(),
            last_modified_revision,
            last_modified_revision_number
        );
    } else {
        lore_debug!(
            "{} not modified in HEAD, walk file history",
            change.path.as_str()
        );
        // Node was not modified in the target LATEST revision, find the
        // previous revision from file history block
        if let Ok(block) = state
            .block_file_metadata(repository.clone(), NodeFileMetadataBlock::index(node_id))
            .await
        {
            let node = block.node(NodeFileMetadata::index(node_id));
            lore_debug!("{} node history {:?}", change.path.as_str(), node);
            if let Ok(state_modified) =
                state::State::deserialize(repository.clone(), node.revision[0]).await
            {
                last_modified_revision = state_modified.revision();
                last_modified_revision_number = state_modified.revision_number();
                lore_debug!(
                    "{} found last modified revision {} -> {}",
                    change.path.as_str(),
                    last_modified_revision_number,
                    last_modified_revision
                );
            } else {
                lore_warn!(
                    "Failed to deserialize last modified state when searching history for {}",
                    change.path.as_str()
                );
            }
        } else {
            lore_warn!(
                "Failed to deserialize file metadata block when searching history for {}",
                change.path.as_str()
            );
        }
    }

    Ok((last_modified_revision, last_modified_revision_number))
}

async fn find_last_merged_revision(
    change: &NodeChange,
    source_branch: BranchId,
    target_branch: BranchId,
    state_branch_point: Arc<State>,
) -> Result<Option<(Hash, u64)>, StateError> {
    let (state_start, node_current) = if change.to.node.is_valid_node_id() {
        (change.to.state.clone(), change.to.node)
    } else {
        // TODO(mjansson): Figure out a better way to store this metadata around file deletions
        lore_debug!("Find last merged revision without to state, iterate revisions");

        let repository = change.to.repository.clone();
        let mut state_current = change.to.state.clone();
        let mut state_parent = state_current.clone();
        loop {
            if let Ok(node_link) = state_current
                .find_node_link(repository.clone(), change.path.as_str())
                .await
            {
                lore_debug!(
                    "Found {} existing in revision {} - {} for last merged",
                    change.path.as_str(),
                    state_current.revision(),
                    state_current.revision_number()
                );

                // If the node was existing in the current state, it means it was deleted in the previous (parent) revision
                if node_link.is_valid() {
                    lore_debug!(
                        "Using {} parent revision {} - {} as last merged start revision",
                        change.path.as_str(),
                        state_parent.revision(),
                        state_parent.revision_number()
                    );
                    break (state_parent, node_link.node);
                }
            }

            if state_current.revision_number() <= state_branch_point.revision_number() {
                lore_debug!(
                    "Did not find node {} last modified, no merged revision",
                    change.path.as_str()
                );
                return Ok(None);
            }

            lore_debug!(
                "Node {} not present in revision {} - {} for last merged start, move to parent",
                change.path.as_str(),
                state_current.revision(),
                state_current.revision_number()
            );

            state_parent = state_current.clone();
            state_current =
                State::deserialize(repository.clone(), state_current.parent_self()).await?;
        }
    };

    // Now walk the history for the file from the target branch LATEST revision and
    // see if we arrive on source branch along any merge, and if that target revision
    // is later than the last modified revision we identified from the source branch file history
    let repository = change.to.repository.clone();
    let mut state_current = state_start.clone();
    while state_current.revision_number() > state_branch_point.revision_number()
        && node_current.is_valid_node_id()
    {
        let node_index = NodeFileMetadataBlock::index(node_current);
        if let Ok(block) = state_current
            .block_file_metadata(repository.clone(), node_index)
            .await
        {
            let node = block.node(NodeFileMetadata::index(node_current));
            if !node.revision[1].is_zero() {
                // TODO(mjansson): Follow merges from other branches bounded by revision number
                // to also catch merges that happen through other branches
                lore_debug!(
                    "{} branch {} revision {} node {} history is a merge, {:?} check if other parent {} is from branch {}",
                    change.path.as_str(),
                    target_branch,
                    node_current,
                    state_current.revision(),
                    node,
                    node.revision[1],
                    source_branch
                );
                if let Ok(state_other) =
                    state::State::deserialize(repository.clone(), node.revision[1]).await
                {
                    if let Ok(state_metadata) =
                        state_other.revision_metadata(repository.clone()).await
                    {
                        if state_metadata.branch == source_branch {
                            lore_debug!(
                                "{} revision merged from branch {} revision {} -> {}",
                                change.path.as_str(),
                                source_branch,
                                state_other.revision(),
                                state_other.revision_number()
                            );
                            return Ok(Some((
                                state_other.revision(),
                                state_other.revision_number(),
                            )));
                        } else {
                            lore_debug!(
                                "{} revision {} -> {} is NOT a merge from branch {}",
                                change.path.as_str(),
                                state_other.revision(),
                                state_other.revision_number(),
                                source_branch,
                            );
                        }
                    } else {
                        lore_warn!("Failed to deserialize other parent state metadata");
                    }
                } else {
                    lore_warn!(
                        "Failed to deserialize other parent state {}",
                        node.revision[1]
                    );
                }
            } else {
                lore_debug!(
                    "{} revision {} node history {:?} is not a merge, continue search in branch node history {}",
                    change.path.as_str(),
                    state_current.revision_number(),
                    node,
                    node.revision[0]
                );
            }

            if let Ok(state_previous) =
                state::State::deserialize(repository.clone(), node.revision[0]).await
            {
                if state_previous.revision_number() < state_branch_point.revision_number() {
                    lore_debug!(
                        "{} stop iterating revisions, reached revision {} < branch point modified revision {}",
                        change.path.as_str(),
                        state_previous.revision_number(),
                        state_branch_point.revision_number()
                    );
                    break;
                } else {
                    lore_debug!(
                        "{} step to revision {} -> {}",
                        change.path.as_str(),
                        state_previous.revision_number(),
                        state_previous.revision()
                    );
                    state_current = state_previous;
                }
            } else {
                lore_warn!("Failed to deserialize state when walking source branch file history");
                break;
            }
        } else {
            lore_warn!(
                "Failed to deserialize file metadata block when walking source branch file history"
            );
            break;
        }
    }

    Ok(None)
}

pub struct TreeResult {
    /// list of paths at this level
    pub paths: Vec<TreePath>,
}

pub async fn tree(
    repository: Arc<RepositoryContext>,
    revision: Hash,
    path: RelativePath,
    max_depth: usize,
    can_read: crate::state::CanReadRepository,
) -> Result<TreeResult, StateError> {
    lore_debug!(
        "Gathering tree in repository {} revision: {} path: {}",
        repository.id,
        revision,
        path.as_str()
    );
    let state = State::deserialize(repository.clone(), revision).await?;
    let paths = gather_tree_paths(state, repository, path, max_depth, can_read).await?;
    Ok(TreeResult { paths })
}

#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreRevisionResolveEventData {
    /// Repository identifier in which repository
    pub repository: RepositoryId,
    /// Identifier of the branch on which resolution is being done
    pub branch: BranchId,
    /// If set to non-empty, the partial hash being resolved
    pub revision: LoreString,
    /// If set to non-zero, the revision number being resolved
    pub revision_number: u64,
    /// Resolving using remote data
    pub remote: u8,
    /// Resolving using local data
    pub local: u8,
}

#[derive(Default)]
pub enum ResolveSearchLocation {
    #[default]
    RemoteOrLocal,
    Remote,
    Local,
}

pub async fn resolve(
    repository: Arc<RepositoryContext>,
    signature: impl AsRef<str>,
    search_limit: Option<usize>,
    search_location: ResolveSearchLocation,
) -> Result<Hash, StateError> {
    let signature = signature.as_ref();
    let original_input = signature.to_string();
    let mut revision = Hash::default();

    let (signature, offset) = if let Some(split) = signature.split_once("~") {
        let prefix = split.0;
        let suffix = split.1;
        if suffix.is_empty() {
            (prefix, Some(1))
        } else {
            let offset: u64 = suffix.parse::<u64>().map_err(|err| {
                lore_debug!("Malformed revision offset {suffix:?}: {err}");
                StateError::from(RevisionNotFound {
                    revision: original_input.clone(),
                })
            })?;

            (prefix, Some(offset))
        }
    } else {
        (signature, None)
    };

    lore_debug!("Resolving signature {signature}, offset {offset:?}");

    let (should_search_remote, should_search_local) = match search_location {
        ResolveSearchLocation::RemoteOrLocal => (true, true),
        ResolveSearchLocation::Remote => (true, false),
        ResolveSearchLocation::Local => (false, true),
    };

    if signature.len() == HASH_STRING_LENGTH && signature.chars().all(|c| c.is_ascii_hexdigit()) {
        revision = Hash::from_str(signature).map_err(|err| {
            lore_debug!("Malformed revision signature {signature:?}: {err}");
            StateError::from(RevisionNotFound {
                revision: original_input.clone(),
            })
        })?;
        lore_debug!("Resolved direct hash signature: {revision}");
    } else if let Some(split) = signature.split_once("@") {
        let prefix = split.0;
        let suffix = split.1;

        lore_debug!("Resolving branch {prefix} signature {signature}");
        let branch = if prefix.is_empty() {
            let (_current_revision, current_branch) =
                crate::instance::load_current_anchor(&repository)
                    .await
                    .forward::<StateError>("Failed deserializing anchor")?;
            current_branch
        } else {
            let branch_status = branch::resolve(repository.clone(), prefix)
                .await
                .map_matched_err("Invalid branch specifier", |m| match m {
                    branch::MatchedBranchError::BranchNotFound(_) => {
                        StateError::from(RevisionNotFound {
                            revision: original_input.clone(),
                        })
                    }
                    other => other.forward::<StateError>("resolving branch for revision"),
                })?;
            branch_status.id
        };

        let remote_latest = if let Ok(remote) = repository.remote().await
            && should_search_remote
        {
            branch::load_remote_latest(remote.clone(), repository.id, branch)
                .await
                .ok()
        } else {
            None
        };

        let local_latest = if should_search_local {
            branch::load_latest(repository.clone(), branch).await.ok()
        } else {
            None
        };

        if suffix.to_uppercase() == "LATEST" || suffix.to_uppercase() == "HEAD" {
            let local = local_latest.filter(|head| !head.is_zero());
            let remote = remote_latest.filter(|head| !head.is_zero());
            revision = match (local, remote) {
                (Some(local), Some(remote)) if local == remote => local,
                (Some(local), Some(remote)) => {
                    match find_branch_point(repository.clone(), remote, local).await {
                        Ok((_branch_point, remote_history, local_history)) => {
                            if local_history.is_empty() && !remote_history.is_empty() {
                                lore_debug!(
                                    "Remote latest {remote} is ahead of local latest {local} and convergent, using remote"
                                );
                                remote
                            } else {
                                local
                            }
                        }
                        Err(err) => {
                            lore_debug!(
                                "Failed to find branch point between local {local} and remote {remote}, falling back to local: {err}"
                            );
                            local
                        }
                    }
                }
                (Some(local), None) => local,
                (None, Some(remote)) => remote,
                (None, None) => Hash::default(),
            };
        } else {
            let revision_number: u64 = suffix.parse::<u64>().map_err(|err| {
                lore_debug!("Invalid revision number {suffix:?}: {err}");
                StateError::from(RevisionNotFound {
                    revision: original_input.clone(),
                })
            })?;

            event::LoreEvent::RevisionResolve(LoreRevisionResolveEventData {
                repository: repository.id,
                branch,
                revision: LoreString::default(),
                revision_number,
                remote: should_search_remote.into(),
                local: should_search_local.into(),
            })
            .send();

            if should_search_remote
                && let Ok(connection) = repository.remote().await
                && let Ok(revision_service) = connection.revision(repository.id).await
                && let Ok(response) = revision_service
                    .revision_list(
                        RevisionListIdentifier {
                            branch,
                            number: revision_number,
                        }
                        .into(),
                    )
                    .await
            {
                if let Ok(item) = response
                    .items
                    .as_slice()
                    .binary_search_by(|item| item.number.cmp(&revision_number))
                {
                    revision = response.items[item].signature;
                    lore_debug!("response revision {}", revision);
                }
                find::cache_revision_list_states(repository.clone(), &response.items).await;
            }

            if revision.is_zero()
                && let Some(head) = remote_latest
                && let Ok(found_revision) =
                    find::revision_by_number(repository.clone(), branch, head, revision_number)
                        .await
            {
                revision = found_revision;
            }
            if revision.is_zero()
                && let Some(head) = local_latest
                && let Ok(found_revision) =
                    find::revision_by_number(repository.clone(), branch, head, revision_number)
                        .await
            {
                revision = found_revision;
            }
        }

        if !revision.is_zero() {
            lore_debug!("Resolved to branch {branch} revision {revision}");
        }
    } else {
        if !signature.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(RevisionNotFound {
                revision: original_input.clone(),
            }
            .into());
        }

        let (_current_revision, current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<StateError>("Failed deserializing anchor")?;
        let branch = current_branch;

        if signature.len() < HASH_STRING_LENGTH {
            event::LoreEvent::RevisionResolve(LoreRevisionResolveEventData {
                repository: repository.id,
                branch,
                revision: signature.into(),
                revision_number: 0,
                remote: should_search_remote.into(),
                local: should_search_local.into(),
            })
            .send();
        }

        if let Ok(found_revision) =
            find::revision_by_string(repository.clone(), branch, signature, search_limit).await
        {
            revision = found_revision;
            lore_debug!("Resolved partial match revision {revision}");
        }
    }

    if revision.is_zero() {
        return Err(RevisionNotFound {
            revision: original_input.clone(),
        }
        .into());
    }

    if let Some(offset) = offset {
        let mut counter = offset;
        while counter > 0 {
            let state = State::deserialize(repository.clone(), revision).await?;
            let parent = state.parent_self();

            if parent.is_zero() {
                return Err(RevisionNotFound {
                    revision: original_input.clone(),
                }
                .into());
            }

            revision = parent;
            counter -= 1;
        }
    }

    Ok(revision)
}
