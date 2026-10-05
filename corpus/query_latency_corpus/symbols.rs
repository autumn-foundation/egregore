use crate::ir::{GraphRecord, NodeKind};

/// Finds a symbol record by name at a specific Git commit.
///
/// `commit` may be a full SHA or a unique prefix from the caller's graph.
#[must_use]
pub fn symbol_at_commit<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    commit: &str,
) -> Option<&'records GraphRecord> {
    symbols_at_commit(records, symbol_name, commit)
        .into_iter()
        .next()
}

/// Returns every symbol record matching `symbol_name` at a specific Git
/// commit, sorted by record ID for deterministic output.
///
/// In a multi-repository store the same name/commit pair can match records in
/// more than one repository; callers that must answer with a single record
/// use the full list to keep the repository boundary visible instead of
/// picking one implicitly (issue #67).
#[must_use]
pub fn symbols_at_commit<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    commit: &str,
) -> Vec<&'records GraphRecord> {
    let mut matches = records
        .iter()
        .filter(|record| matches_symbol_at_commit(record, symbol_name, commit))
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id().cmp(right.id()));
    matches
}

/// Returns `true` when a symbol `name` matches a partial-name `pattern`
/// (issue #102).
///
/// Semantics (deterministic, no regex engine):
///
/// - A pattern containing `*` is an **anchored glob** over the whole name:
///   each `*` matches any (possibly empty) run of characters and every other
///   character is literal. `handle_*` is a prefix match, `*_sink` a suffix
///   match, and a starless glob would be an exact match.
/// - A pattern without `*` matches as a **literal substring** anywhere in the
///   name.
/// - Matching is case-sensitive unless `case_insensitive` is set, in which
///   case both sides are Unicode-lowercased first.
#[must_use]
pub fn symbol_name_matches(pattern: &str, name: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        return symbol_name_matches(&pattern.to_lowercase(), &name.to_lowercase(), false);
    }
    if !pattern.contains('*') {
        return name.contains(pattern);
    }
    glob_matches(pattern, name)
}

/// Anchored `*`-glob match: `pattern` must cover the whole of `name`.
///
/// Standard greedy algorithm: the segment before the first `*` must be a
/// prefix, the segment after the last `*` must be a non-overlapping suffix,
/// and the middle segments must appear in order (earliest match) in between.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    let (first, rest_segments) = segments.split_first().expect("split yields >= 1 segment");
    if rest_segments.is_empty() {
        // No `*` in the pattern; anchored means exact.
        return name == *first;
    }
    let Some(core) = name.strip_prefix(first) else {
        return false;
    };
    let (last, middle) = rest_segments.split_last().expect("checked non-empty");
    let Some(mut core) = core.strip_suffix(last) else {
        return false;
    };
    for segment in middle {
        match core.find(segment) {
            Some(idx) => core = &core[idx + segment.len()..],
            None => return false,
        }
    }
    true
}

fn matches_symbol_at_commit(record: &GraphRecord, symbol_name: &str, commit: &str) -> bool {
    let GraphRecord::Node {
        kind,
        name,
        temporal,
        ..
    } = record
    else {
        return false;
    };
    *kind == NodeKind::Symbol
        && name.as_deref() == Some(symbol_name)
        && temporal
            .as_ref()
            .is_some_and(|temporal| temporal.git_commit.starts_with(commit))
}

// ── symbol_context ────────────────────────────────────────────────────────────
