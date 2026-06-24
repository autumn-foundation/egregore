#![allow(clippy::derive_partial_eq_without_eq, clippy::cast_precision_loss)]

use crate::ir::{EdgeLabel, GraphRecord, NodeKind};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Configuration for the memory health audit subcommand.
#[derive(Debug, Clone)]
pub struct MemoryHealthConfig {
    /// Minimum fraction of observation records that must have provenance coverage.
    pub min_provenance_coverage: f64,
    /// Maximum fraction of observation records that can have dangling evidence.
    pub max_dangling_evidence: f64,
    /// Optional maximum fraction of observation records that can be unverified.
    pub max_unverified: Option<f64>,
    /// Optional maximum fraction of non-tombstoned observation records that can be stale/disputed/unratified/orphaned.
    pub max_current_guidance_contamination: Option<f64>,
}

/// Numerator, denominator, and computed ratio for a health metric.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryHealthCounts {
    /// Numerator (count matching the metric).
    pub numerator: usize,
    /// Denominator (total population).
    pub denominator: usize,
    /// Computed ratio.
    pub ratio: f64,
}

/// Age distribution of observation records.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgeDistribution {
    /// Oldest observation timestamp.
    pub oldest: Option<String>,
    /// Newest observation timestamp.
    pub newest: Option<String>,
    /// Bucketed counts keyed by YYYY-MM.
    pub buckets: BTreeMap<String, usize>,
}

/// Diagnostic representing a threshold gate breach.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryHealthDiagnostic {
    /// Stable code identifying the gate breach (e.g. `provenance_coverage_below_threshold`).
    pub code: String,
    /// Value of the configured threshold.
    pub threshold: f64,
    /// Observed value.
    pub observed: f64,
    /// Human-readable diagnostic message.
    pub message: String,
}

/// Completed agent-memory health report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MemoryHealthReport {
    /// True if all configured thresholds were met; false otherwise.
    pub ok: bool,
    /// Total count of observation nodes in the graph.
    pub total_observations: usize,
    /// Provenance coverage metrics.
    pub provenance_coverage: MemoryHealthCounts,
    /// Unverified claim metrics.
    pub unverified: MemoryHealthCounts,
    /// Superseded claim metrics.
    pub superseded: MemoryHealthCounts,
    /// Contradicted claim metrics.
    pub contradicted: MemoryHealthCounts,
    /// Dangling evidence metrics.
    pub dangling_evidence: MemoryHealthCounts,
    /// Contamination of the current guidance pool.
    pub current_guidance_contamination: MemoryHealthCounts,
    /// Count of records missing provenance altogether.
    pub missing_provenance: MemoryHealthCounts,
    /// Count of records with a source handle but whose evidence does not resolve.
    pub weak_provenance: MemoryHealthCounts,
    /// Count of unverified, but fully citable, provisional hypothesis records.
    pub unratified_memory: MemoryHealthCounts,
    /// Count of historically valid records that are superseded or contradicted.
    pub stale_or_contradicted_memory: MemoryHealthCounts,
    /// Concentration of records per source transcript handle.
    pub source_transcript_concentration: BTreeMap<String, usize>,
    /// Age distribution statistics.
    pub age_distribution: AgeDistribution,
    /// Tripped threshold diagnostics.
    pub diagnostics: Vec<MemoryHealthDiagnostic>,
}

/// Run the agent-memory health report audit over a set of graph records.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn run_memory_health_audit(
    records: &[GraphRecord],
    config: &MemoryHealthConfig,
) -> MemoryHealthReport {
    let mut by_id = BTreeMap::new();
    let mut tombstoned = BTreeSet::new();

    // First pass: collect nodes and tombstones
    for r in records {
        match r {
            GraphRecord::Node { id, .. } => {
                by_id.insert(id.as_str(), r);
            }
            GraphRecord::Tombstone { deleted_id, .. } => {
                tombstoned.insert(deleted_id.as_str());
            }
            GraphRecord::Edge { .. } => {}
        }
    }

    // Second pass: collect non-tombstoned edges
    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    for r in records {
        match r {
            GraphRecord::Edge { id, label, source, target, .. } if !tombstoned.contains(id.as_str()) => {
                edges_from
                    .entry(source.as_str())
                    .or_default()
                    .push((label, target.as_str()));
            }
            _ => {}
        }
    }

    // Build the temporal resolver for supersession/contradiction
    let resolver = crate::temporal_status::TemporalResolver::build(records);

    // Collect active (non-tombstoned) latest Observation nodes
    let active_observations: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                kind: NodeKind::Observation,
                id,
                ..
            } = r
            {
                if tombstoned.contains(id.as_str()) {
                    return false;
                }
                // Only process the latest version of this Observation node
                by_id
                    .get(id.as_str())
                    .is_some_and(|latest| std::ptr::eq(*latest, *r))
            } else {
                false
            }
        })
        .collect();

    let total = active_observations.len();

    let mut prov_cov_count = 0;
    let mut unverified_count = 0;
    let mut superseded_count = 0;
    let mut contradicted_count = 0;
    let mut dangling_count = 0;
    let mut missing_prov_count = 0;
    let mut weak_prov_count = 0;
    let mut unratified_count = 0;
    let mut stale_contra_count = 0;
    let mut contamination_count = 0;

    let mut concentration = BTreeMap::new();
    let mut timestamps = Vec::new();
    let mut age_buckets = BTreeMap::new();

    // Helper to check if a node has dangling evidence
    let check_dangling = |obs: &GraphRecord| -> bool {
        if let GraphRecord::Node {
            id,
            evidence_links,
            superseded_by,
            ..
        } = obs
        {
            if let Some(links) = evidence_links {
                for link in links {
                    if let Some(target_id) = &link.target_record_id {
                        if tombstoned.contains(target_id.as_str())
                            || !by_id.contains_key(target_id.as_str())
                        {
                            return true;
                        }
                    } else {
                        // Triple-only target counts as unresolved
                        return true;
                    }
                }
            }
            if superseded_by
                .as_ref()
                .filter(|s| !s.is_empty())
                .is_some_and(|sup_id| {
                    tombstoned.contains(sup_id.as_str()) || !by_id.contains_key(sup_id.as_str())
                })
            {
                return true;
            }
            if let Some(edges) = edges_from.get(id.as_str()) {
                for (_label, target) in edges {
                    // Any outbound edge from an active observation must point to a live node
                    if tombstoned.contains(target) || !by_id.contains_key(target) {
                        return true;
                    }
                }
            }
        }
        false
    };

    for obs in &active_observations {
        let id = obs.id();
        let GraphRecord::Node {
            agent_id,
            session_id,
            source_handle,
            observed_at,
            ..
        } = obs
        else {
            unreachable!()
        };

        // 1. Provenance Coverage
        let has_prov = (agent_id.is_some() || session_id.is_some())
            && source_handle.is_some()
            && observed_at.is_some();
        if has_prov {
            prov_cov_count += 1;
        } else {
            missing_prov_count += 1;
        }

        // 2. Verification Status
        let verified = crate::query::is_verified_claim(obs, &by_id, &edges_from, &tombstoned);
        if !verified {
            unverified_count += 1;
        }

        // 3. Temporal Status (Superseded/Contradicted)
        let (status, _, _) = resolver.resolve_status(id);
        let is_sup = status == "superseded" || status == "cycle";
        let is_contra = status == "contradicted";

        if is_sup {
            superseded_count += 1;
        }
        if is_contra {
            contradicted_count += 1;
        }

        // 4. Dangling Evidence
        let is_dang = check_dangling(obs);
        if is_dang {
            dangling_count += 1;
        }

        // 5. Weak Provenance (has source handle but dangling)
        if source_handle.is_some() && is_dang {
            weak_prov_count += 1;
        }

        // 6. Unratified Memory (provenance exists but unverified)
        if has_prov && !verified {
            unratified_count += 1;
        }

        // 7. Stale or Contradicted
        if is_sup || is_contra {
            stale_contra_count += 1;
        }

        // 8. Guidance Contamination (limited to CURRENT guidance; ignore if already superseded)
        if !is_sup && (is_contra || !verified || !has_prov) {
            contamination_count += 1;
        }

        // Concentration
        if let Some(sh) = source_handle {
            *concentration.entry(sh.clone()).or_insert(0) += 1;
        }

        // Age distribution with robust UTC normalization
        if let Some(ts) = observed_at {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                let utc_dt = dt.with_timezone(&chrono::Utc);
                timestamps.push(utc_dt);
                let bucket_key = utc_dt.format("%Y-%m").to_string();
                *age_buckets.entry(bucket_key).or_insert(0) += 1;
            } else {
                // Fallback for non-compliant/unparseable timestamps that still match YYYY-MM prefix
                if ts.len() >= 7
                    && ts.as_bytes()[0..4].iter().all(u8::is_ascii_digit)
                    && ts.as_bytes()[4] == b'-'
                    && ts.as_bytes()[5..7].iter().all(u8::is_ascii_digit)
                {
                    *age_buckets.entry(ts[0..7].to_owned()).or_insert(0) += 1;
                }
            }
        }
    }

    let make_counts = |num: usize| -> MemoryHealthCounts {
        MemoryHealthCounts {
            numerator: num,
            denominator: total,
            ratio: if total > 0 {
                num as f64 / total as f64
            } else {
                0.0
            },
        }
    };

    // Current guidance denominator excludes superseded records
    let current_guidance_total = total.saturating_sub(superseded_count);
    let current_guidance_contamination = MemoryHealthCounts {
        numerator: contamination_count,
        denominator: current_guidance_total,
        ratio: if current_guidance_total > 0 {
            contamination_count as f64 / current_guidance_total as f64
        } else {
            0.0
        },
    };

    let provenance_coverage = make_counts(prov_cov_count);
    let unverified = make_counts(unverified_count);
    let superseded = make_counts(superseded_count);
    let contradicted = make_counts(contradicted_count);
    let dangling_evidence = make_counts(dangling_count);
    let missing_provenance = make_counts(missing_prov_count);
    let weak_provenance = make_counts(weak_prov_count);
    let unratified_memory = make_counts(unratified_count);
    let stale_or_contradicted_memory = make_counts(stale_contra_count);

    timestamps.sort();
    let oldest = timestamps
        .first()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let newest = timestamps
        .last()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    let mut diagnostics = Vec::new();
    let mut ok = true;

    if total == 0 {
        ok = false;
        diagnostics.push(MemoryHealthDiagnostic {
            code: "no_memory".to_owned(),
            threshold: 0.0,
            observed: 0.0,
            message: "No active memory records found in the agent-memory domain".to_owned(),
        });
    } else {
        // Evaluate gate thresholds
        if provenance_coverage.ratio < config.min_provenance_coverage {
            ok = false;
            diagnostics.push(MemoryHealthDiagnostic {
                code: "provenance_coverage_below_threshold".to_owned(),
                threshold: config.min_provenance_coverage,
                observed: provenance_coverage.ratio,
                message: format!(
                    "provenance-coverage ratio {:.4} is below threshold {:.4}",
                    provenance_coverage.ratio, config.min_provenance_coverage
                ),
            });
        }
        if dangling_evidence.ratio > config.max_dangling_evidence {
            ok = false;
            diagnostics.push(MemoryHealthDiagnostic {
                code: "dangling_evidence_above_threshold".to_owned(),
                threshold: config.max_dangling_evidence,
                observed: dangling_evidence.ratio,
                message: format!(
                    "dangling-evidence ratio {:.4} is above threshold {:.4}",
                    dangling_evidence.ratio, config.max_dangling_evidence
                ),
            });
        }
        if let Some(threshold) = config.max_unverified.filter(|&t| unverified.ratio > t) {
            ok = false;
            diagnostics.push(MemoryHealthDiagnostic {
                code: "unverified_above_threshold".to_owned(),
                threshold,
                observed: unverified.ratio,
                message: format!(
                    "unverified ratio {:.4} is above threshold {:.4}",
                    unverified.ratio, threshold
                ),
            });
        }
        if let Some(threshold) = config
            .max_current_guidance_contamination
            .filter(|&t| current_guidance_contamination.ratio > t)
        {
            ok = false;
            diagnostics.push(MemoryHealthDiagnostic {
                code: "current_guidance_contamination_above_threshold".to_owned(),
                threshold,
                observed: current_guidance_contamination.ratio,
                message: format!(
                    "current guidance contamination ratio {:.4} is above threshold {:.4}",
                    current_guidance_contamination.ratio, threshold
                ),
            });
        }
    }

    MemoryHealthReport {
        ok,
        total_observations: total,
        provenance_coverage,
        unverified,
        superseded,
        contradicted,
        dangling_evidence,
        current_guidance_contamination,
        missing_provenance,
        weak_provenance,
        unratified_memory,
        stale_or_contradicted_memory,
        source_transcript_concentration: concentration,
        age_distribution: AgeDistribution {
            oldest,
            newest,
            buckets: age_buckets,
        },
        diagnostics,
    }
}
