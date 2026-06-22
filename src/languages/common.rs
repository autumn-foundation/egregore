//! Language-neutral extraction helpers shared by the per-language extractors.

use tree_sitter::Node;

use crate::ir::SourceSpan;

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
#[must_use]
pub fn looks_like_call(text: &str, name: &str) -> bool {
    let simple_name = name.rsplit("::").next().unwrap_or(name);
    let simple_name = simple_name.rsplit('.').next().unwrap_or(simple_name);
    let direct = format!("{simple_name}(");
    let associated = format!("::{simple_name}(");
    let method = format!(".{simple_name}(");
    text.contains(&direct) || text.contains(&associated) || text.contains(&method)
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
}
