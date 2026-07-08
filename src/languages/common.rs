//! Language-neutral extraction helpers shared by the per-language extractors.

use std::collections::BTreeMap;

use tree_sitter::Node;

use crate::ir::{EdgeLabel, Graph, GraphRecord, SourceSpan};

/// A recorded symbol body used to emit cross-symbol reference edges.
#[derive(Debug, Clone)]
pub struct SymbolBody {
    /// Stable record ID for the symbol node.
    pub id: String,
    /// Qualified name of the symbol.
    pub name: String,
    /// Source text of the symbol body.
    pub text: String,
}

/// Pushes a single typed edge onto the graph.
pub fn add_graph_edge(
    graph: &mut Graph,
    label: EdgeLabel,
    source: String,
    target: String,
    summary: String,
) {
    graph.push(GraphRecord::edge(
        label,
        source,
        target,
        Some("1.0".to_owned()),
        summary,
    ));
}

/// Returns the next source-order ordinal for a (kind, name) pair and advances the counter.
///
/// Ordinals start at 0 and increase monotonically per pair so
/// same-named symbols in the same file get distinct stable IDs.
pub fn next_symbol_ordinal(
    ordinals: &mut BTreeMap<(String, String), u64>,
    symbol_kind: &str,
    qualified_name: &str,
) -> u64 {
    let key = (symbol_kind.to_owned(), qualified_name.to_owned());
    let slot = ordinals.entry(key).or_default();
    let current = *slot;
    *slot += 1;
    current
}

/// Emits `Calls` / `References` edges between symbol bodies and the
/// definitions visible in the same file.
///
/// The heuristic is deterministic: a body text that contains a definition name
/// as a standalone identifier and also looks like a call site (`name(`,
/// `::name(`, `.name(`) gets a `Calls` edge; any other identifier reference
/// gets a `References` edge. Self-references (body ID == target ID) and
/// name-equality guard loops (name == body name) are skipped.
///
/// Precision contract (issue #134): callers must supply [`SymbolBody::text`]
/// built by [`reference_text`], so names that appear only inside comments or
/// string literals never produce an edge, and substring occurrences never
/// classify as calls ([`contains_identifier`] / [`looks_like_call`] both
/// require identifier token boundaries).
pub fn emit_reference_edges(
    graph: &mut Graph,
    definitions: &BTreeMap<String, String>,
    bodies: &[SymbolBody],
) {
    for body in bodies {
        for (name, target_id) in definitions {
            if body.id == *target_id || name == &body.name || !contains_identifier(&body.text, name)
            {
                continue;
            }
            if looks_like_call(&body.text, name) {
                add_graph_edge(
                    graph,
                    EdgeLabel::Calls,
                    body.id.clone(),
                    target_id.clone(),
                    format!("{} calls {name}", body.name),
                );
            } else {
                add_graph_edge(
                    graph,
                    EdgeLabel::References,
                    body.id.clone(),
                    target_id.clone(),
                    format!("{} references {name}", body.name),
                );
            }
        }
    }
}

/// Returns a symbol body's source text with comment and string-literal
/// content removed, for same-file reference-edge matching (issue #134).
///
/// Every descendant node (named or anonymous, including tokens inside macro
/// token trees) whose kind appears in `excluded_kinds` is replaced by a single
/// space, so a definition name that occurs only inside a comment, a string
/// literal, or another excluded node can never satisfy
/// [`contains_identifier`] or [`looks_like_call`]. The exclusion list is
/// per-language because Tree-sitter grammars name their comment and literal
/// nodes differently. Output is deterministic: ranges come from one pre-order
/// AST walk that never descends into an excluded node.
#[must_use]
pub fn reference_text(node: Node<'_>, source: &str, excluded_kinds: &[&str]) -> String {
    let mut excluded_ranges: Vec<(usize, usize)> = Vec::new();
    collect_excluded_ranges(node, excluded_kinds, &mut excluded_ranges);
    let start = node.start_byte();
    let text = &source[start..node.end_byte()];
    if excluded_ranges.is_empty() {
        return text.to_owned();
    }
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (range_start, range_end) in excluded_ranges {
        let relative_start = range_start - start;
        let relative_end = range_end - start;
        if relative_start > cursor {
            result.push_str(&text[cursor..relative_start]);
        }
        result.push(' ');
        cursor = cursor.max(relative_end);
    }
    if cursor < text.len() {
        result.push_str(&text[cursor..]);
    }
    result
}

/// Collects the byte ranges of descendant nodes whose kind is excluded from
/// reference matching, in source order, without descending into matches.
fn collect_excluded_ranges(
    node: Node<'_>,
    excluded_kinds: &[&str],
    excluded_ranges: &mut Vec<(usize, usize)>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if excluded_kinds.contains(&child.kind()) {
            excluded_ranges.push((child.start_byte(), child.end_byte()));
        } else {
            collect_excluded_ranges(child, excluded_kinds, excluded_ranges);
        }
    }
}

/// Reads the text of a Tree-sitter node as a trimmed, non-empty string.
#[must_use]
pub fn identifier_text(node: Node<'_>, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes())
        .ok()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

/// Reads the `name` field of a declaration node as trimmed text.
#[must_use]
pub fn node_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|n| identifier_text(n, source))
}

/// Collapses runs of whitespace to a single space and trims the ends.
#[must_use]
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Splits a repo-relative path on `/` and `\`, discarding empty segments.
///
/// Used by per-language module-path helpers to canonicalize path traversal.
#[must_use]
pub fn path_segments(repo_relative_path: &str) -> Vec<String> {
    repo_relative_path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Returns all descendant nodes of the given kind (depth-first, pre-order).
///
/// Stops recursing into a subtree once a matching node is found at that level,
/// so children of a matched node are not collected as additional matches.
#[must_use]
pub fn descendant_kinds<'tree>(node: Node<'tree>, kind: &str) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            found.push(child);
        } else {
            found.extend(descendant_kinds(child, kind));
        }
    }
    found
}

/// Normalizes C-like source by stripping `//` and `/* */` comments, collapsing
/// whitespace, and preserving string and character literal contents.
///
/// Delimiters listed in `raw_delims` are treated as raw-string openers —
/// backslash escapes inside them are passed through unchanged (e.g. Go's
/// backtick raw strings). Pass `&[]` for languages where all quoted delimiters
/// process escapes (TypeScript), or `&['\x60']` for Go (backtick = raw).
#[must_use]
pub fn normalize_c_like_code(code: &str, raw_delims: &[char]) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    let mut last_pushed: Option<char> = None;

    let chars: Vec<char> = code.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let c = chars[i];

        // Line comment: // … \n
        if c == '/' && i + 1 < len && chars[i + 1] == '/' {
            i += 2;
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            pending_space = true;
            continue;
        }

        // Block comment: /* … */
        if c == '/' && i + 1 < len && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2;
            } else {
                i = len;
            }
            pending_space = true;
            continue;
        }

        // String / character literals: delim opens, matching delim closes.
        if c == '"' || c == '\'' || c == '`' {
            if pending_space {
                pending_space = false;
                let is_current_ident = c.is_alphanumeric() || c == '_';
                let is_last_ident =
                    last_pushed.is_some_and(|last| last.is_alphanumeric() || last == '_');
                if is_current_ident && is_last_ident {
                    result.push(' ');
                }
            }
            let delim = c;
            let raw = raw_delims.contains(&delim);
            result.push(c);
            last_pushed = Some(c);
            i += 1;
            let mut escaped = false;
            while i < len {
                let sc = chars[i];
                result.push(sc);
                last_pushed = Some(sc);
                i += 1;
                if escaped {
                    escaped = false;
                } else if !raw && sc == '\\' {
                    escaped = true;
                } else if sc == delim {
                    break;
                }
            }
            continue;
        }

        if c.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space {
                pending_space = false;
                let is_current_ident = c.is_alphanumeric() || c == '_';
                let is_last_ident =
                    last_pushed.is_some_and(|last| last.is_alphanumeric() || last == '_');
                if is_current_ident && is_last_ident {
                    result.push(' ');
                }
            }
            result.push(c);
            last_pushed = Some(c);
        }
        i += 1;
    }
    result.trim().to_owned()
}

/// Builds a [`SourceSpan`] from a Tree-sitter node's byte and line positions.
///
/// Line numbers are 1-based to match editor conventions.
#[must_use]
pub fn span(node: Node<'_>) -> SourceSpan {
    SourceSpan {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True when `text` invokes `name` — i.e. the (possibly path-qualified) simple
/// name is immediately followed by `(`, as a direct call `name(`, an associated
/// call `::name(`, or a method call `.name(`.
///
/// The occurrence must start at an identifier token boundary, so `run(` inside
/// `prerun(` never classifies a mention of `run` as a call (issue #134).
#[must_use]
pub fn looks_like_call(text: &str, name: &str) -> bool {
    let simple_name = name.rsplit("::").next().unwrap_or(name);
    let simple_name = simple_name.rsplit('.').next().unwrap_or(simple_name);
    if simple_name.is_empty() {
        return false;
    }
    let needle = format!("{simple_name}(");
    let bytes = text.as_bytes();
    text.match_indices(&needle)
        .any(|(idx, _)| idx == 0 || !is_ident_byte(bytes[idx - 1]))
}

/// True when `name` occurs in `text` as a standalone identifier.
///
/// Each occurrence must not be flanked by identifier characters. Avoids substring
/// false positives such as `Error` matching inside `ParseError`.
#[must_use]
pub fn contains_identifier(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let nlen = name.len();
    // `match_indices` yields byte offsets at valid char boundaries, so no manual
    // slicing can split a multi-byte UTF-8 character (Unicode identifiers would
    // otherwise panic during a scan). A non-identifier flanking byte — including
    // any UTF-8 continuation/lead byte — counts as a token boundary.
    text.match_indices(name).any(|(idx, _)| {
        let before_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
        let end = idx + nlen;
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        before_ok && after_ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_identifier_requires_token_boundaries() {
        // Whole-identifier matches are accepted, including qualified paths.
        assert!(contains_identifier("let x: Error = make();", "Error"));
        assert!(contains_identifier("foo::Error::new()", "Error"));
        assert!(contains_identifier("-> Widget {", "Widget"));
        // Substrings of a larger identifier are rejected.
        assert!(!contains_identifier("let e: ParseError = x;", "Error"));
        assert!(!contains_identifier("Errorhandler::run()", "Error"));
        assert!(!contains_identifier("my_widget", "widget"));
        // A multi-byte Unicode identifier appearing only inside a larger
        // identifier must be rejected without panicking on a char boundary.
        assert!(!contains_identifier("xéx", "é"));
        assert!(contains_identifier("call(é)", "é"));
    }

    #[test]
    fn looks_like_call_detects_dotted_and_pathed_invocations() {
        assert!(looks_like_call("foo()", "foo"));
        assert!(looks_like_call("obj.method()", "method"));
        assert!(looks_like_call("Type::assoc()", "assoc"));
        assert!(looks_like_call("pkg.mod.func()", "func"));
        assert!(!looks_like_call("let x = foo;", "foo"));
    }

    #[test]
    fn looks_like_call_requires_identifier_boundary() {
        // `run(` inside `prerun(` must not classify `run` as called (issue #134).
        assert!(!looks_like_call("prerun()", "run"));
        assert!(!looks_like_call("let x = run; prerun()", "run"));
        assert!(looks_like_call("let x = prerun; run()", "run"));
        assert!(looks_like_call("(run())", "run"));
        assert!(!looks_like_call("anything", ""));
    }
}
