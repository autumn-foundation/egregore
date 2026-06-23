use crate::ir::{EdgeLabel, GraphRecord};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Supersession resolution mode for memory queries.
#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupersessionMode {
    /// Exclude superseded/contradicted records entirely, returning them in the `excluded` section.
    Exclude,
    /// Return all matching records, but flag superseded/contradicted records with their forward handles.
    IncludeButFlag,
}

/// A reference to another temporal record.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TemporalReference {
    /// Stable record ID.
    pub record_id: String,
    /// Citable handle (e.g. `agent_id:session_id` or `agent_id`).
    pub handle: String,
}

/// A resolver to query supersession chains and contradiction graphs.
pub struct TemporalResolver<'a> {
    // Direct superseding links: older node ID -> set of newer node IDs
    superseded_by: HashMap<&'a str, HashSet<&'a str>>,
    // Contradicts links: node ID -> set of contradicting node IDs
    contradicts: HashMap<&'a str, HashSet<&'a str>>,
    // Map of record ID -> GraphRecord reference to retrieve handles and details
    records_by_id: HashMap<&'a str, &'a GraphRecord>,
}

impl<'a> TemporalResolver<'a> {
    /// Build the resolver from a slice of graph records.
    #[must_use]
    pub fn build(records: &'a [GraphRecord]) -> Self {
        let mut superseded_by: HashMap<&'a str, HashSet<&'a str>> = HashMap::new();
        let mut contradicts: HashMap<&'a str, HashSet<&'a str>> = HashMap::new();
        let mut records_by_id: HashMap<&'a str, &'a GraphRecord> = HashMap::new();

        // First pass: collect all nodes by ID
        for r in records {
            if let GraphRecord::Node { id, .. } = r {
                records_by_id.insert(id.as_str(), r);
            }
        }

        // Second pass: extract relationships from nodes
        for r in records {
            match r {
                GraphRecord::Node {
                    id,
                    superseded_by: Some(sub_by),
                    evidence_links,
                    ..
                } => {
                    if !sub_by.is_empty() {
                        superseded_by
                            .entry(id.as_str())
                            .or_default()
                            .insert(sub_by.as_str());
                    }
                    if let Some(links) = evidence_links {
                        for link in links {
                            if let Some(target_id) = &link.target_record_id {
                                if link.relation == "SUPERSEDES" {
                                    // id SUPERSEDES target_id => target_id is superseded by id
                                    superseded_by
                                        .entry(target_id.as_str())
                                        .or_default()
                                        .insert(id.as_str());
                                } else if link.relation == "CONTRADICTS" {
                                    contradicts
                                        .entry(id.as_str())
                                        .or_default()
                                        .insert(target_id.as_str());
                                    contradicts
                                        .entry(target_id.as_str())
                                        .or_default()
                                        .insert(id.as_str());
                                }
                            }
                        }
                    }
                }
                GraphRecord::Node {
                    id,
                    evidence_links: Some(links),
                    ..
                } => {
                    for link in links {
                        if let Some(target_id) = &link.target_record_id {
                            if link.relation == "SUPERSEDES" {
                                // id SUPERSEDES target_id => target_id is superseded by id
                                superseded_by
                                    .entry(target_id.as_str())
                                    .or_default()
                                    .insert(id.as_str());
                            } else if link.relation == "CONTRADICTS" {
                                contradicts
                                    .entry(id.as_str())
                                    .or_default()
                                    .insert(target_id.as_str());
                                contradicts
                                    .entry(target_id.as_str())
                                    .or_default()
                                    .insert(id.as_str());
                            }
                        }
                    }
                }
                GraphRecord::Edge {
                    label,
                    source,
                    target,
                    ..
                } => {
                    if *label == EdgeLabel::Supersedes {
                        // source SUPERSEDES target => target is superseded by source
                        superseded_by
                            .entry(target.as_str())
                            .or_default()
                            .insert(source.as_str());
                    } else if *label == EdgeLabel::Contradicts {
                        contradicts
                            .entry(source.as_str())
                            .or_default()
                            .insert(target.as_str());
                        contradicts
                            .entry(target.as_str())
                            .or_default()
                            .insert(source.as_str());
                    }
                }
                _ => {}
            }
        }

        Self {
            superseded_by,
            contradicts,
            records_by_id,
        }
    }

    /// Helper to get the handle for a given record.
    #[must_use]
    pub fn get_handle(&self, record_id: &str) -> String {
        if let Some(GraphRecord::Node {
            agent_id,
            session_id,
            source_handle,
            ..
        }) = self.records_by_id.get(record_id)
        {
            if let (Some(a), Some(s)) = (agent_id.as_deref(), session_id.as_deref()) {
                format!("{a}:{s}")
            } else if let Some(a) = agent_id.as_deref() {
                a.to_owned()
            } else if let Some(sh) = source_handle.as_deref() {
                sh.to_owned()
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    }

    /// Resolves the supersession heads for a record transitively.
    ///
    /// # Errors
    /// Returns a `HashSet` containing the nodes involved in a cycle if a cycle is detected.
    pub fn resolve_supersession_heads(
        &self,
        start_id: &'a str,
    ) -> Result<Option<HashSet<&'a str>>, HashSet<&'a str>> {
        if !self.superseded_by.contains_key(start_id) {
            return Ok(None);
        }

        let mut heads = HashSet::new();
        // DFS stack stores (current_node, path_from_start)
        let mut stack = vec![(start_id, vec![start_id])];

        while let Some((curr, path)) = stack.pop() {
            if let Some(next_set) = self.superseded_by.get(curr) {
                let mut is_leaf = true;
                for next in next_set {
                    if path.contains(next) {
                        // Cycle detected!
                        let mut cycle = HashSet::new();
                        let pos = path.iter().position(|x| x == next).unwrap_or(0);
                        for item in &path[pos..] {
                            cycle.insert(*item);
                        }
                        cycle.insert(*next);
                        return Err(cycle);
                    }
                    is_leaf = false;
                    let mut next_path = path.clone();
                    next_path.push(*next);
                    stack.push((*next, next_path));
                }
                if is_leaf {
                    heads.insert(curr);
                }
            } else {
                heads.insert(curr);
            }
        }

        heads.remove(start_id);

        if heads.is_empty() {
            Ok(None)
        } else {
            Ok(Some(heads))
        }
    }

    /// Resolve status, supersession heads, and contradictions for a given record.
    ///
    /// Returns:
    /// `(status, superseded_by_refs, contradicted_by_refs)`
    ///
    /// Where status is one of: `"current"`, `"superseded"`, `"contradicted"`, `"cycle"`.
    #[must_use]
    pub fn resolve_status(
        &self,
        id: &'a str,
    ) -> (&'static str, Vec<TemporalReference>, Vec<TemporalReference>) {
        match self.resolve_supersession_heads(id) {
            Err(_) => ("cycle", vec![], vec![]),
            Ok(Some(heads)) => {
                let mut refs = Vec::new();
                for head_id in heads {
                    if head_id != id {
                        let handle = self.get_handle(head_id);
                        refs.push(TemporalReference {
                            record_id: head_id.to_string(),
                            handle,
                        });
                    }
                }
                refs.sort_by(|a, b| a.record_id.cmp(&b.record_id));
                ("superseded", refs, vec![])
            }
            Ok(None) => {
                // Check contradictions
                if let Some(contradicting_ids) =
                    self.contradicts.get(id).filter(|ids| !ids.is_empty())
                {
                    let mut refs = Vec::new();
                    for contra_id in contradicting_ids {
                        let handle = self.get_handle(contra_id);
                        refs.push(TemporalReference {
                            record_id: (*contra_id).to_string(),
                            handle,
                        });
                    }
                    refs.sort_by(|a, b| a.record_id.cmp(&b.record_id));
                    return ("contradicted", vec![], refs);
                }
                ("current", vec![], vec![])
            }
        }
    }
}
