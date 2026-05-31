//! GitHub-token scrubber for operator-facing output.
//!
//! Per `docs/schema/import-github.md` §2, every stdout/stderr/log line the
//! importer emits MUST pass through a scrubber that replaces any GitHub token
//! pattern with `[REDACTED_GH_TOKEN]` before the bytes leave the process. This
//! is a separate surface from the persisted-record redaction policy
//! (`docs/schema/redaction.md`): the scrubber guards *diagnostics*, the
//! redaction policy guards *persisted fields*.
//!
//! Two patterns are scrubbed:
//! - Classic/OAuth PATs: `gh[pousr]_[A-Za-z0-9]{36,}`
//! - Fine-grained PATs: `github_pat_[A-Za-z0-9_]{36,}`

/// Replacement marker emitted in place of a detected token.
pub const SCRUB_MARKER: &str = "[REDACTED_GH_TOKEN]";

/// Minimum body length (characters after the prefix) for a match.
const MIN_TOKEN_BODY: usize = 36;

/// Returns `value` with every GitHub-token-shaped substring replaced by
/// [`SCRUB_MARKER`].
///
/// The scan is greedy on the token body so the entire secret is replaced, not
/// just its prefix. Non-token text is preserved byte-for-byte.
#[must_use]
pub fn scrub(value: &str) -> String {
    // Fast path: a token prefix always contains "gh" (`ghp_`/`gho_`/… or
    // `github_pat_`). Note `github` itself has no "gh" substring, so the guard
    // must check for the literal prefixes, not the bigram "gh".
    if !value.contains("gh") && !value.contains("github_pat_") {
        return value.to_owned();
    }
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(end) = match_token(value, i) {
            out.push_str(SCRUB_MARKER);
            i = end;
        } else {
            // Push one UTF-8 char to keep the output valid.
            let ch = value[i..].chars().next().expect("non-empty slice");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Attempts to match a token starting at byte offset `start`.
///
/// Returns the exclusive end offset of the matched token, or `None`.
fn match_token(value: &str, start: usize) -> Option<usize> {
    let rest = &value[start..];
    // Fine-grained PAT: github_pat_ + [A-Za-z0-9_]{36,}
    if let Some(after) = rest.strip_prefix("github_pat_") {
        let len = run_len(after, |c| c.is_ascii_alphanumeric() || c == '_');
        if len >= MIN_TOKEN_BODY {
            return Some(start + "github_pat_".len() + len);
        }
        return None;
    }
    // Classic/OAuth PAT: gh[pousr]_ + [A-Za-z0-9]{36,}
    const CLASSIC: &[&str] = &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
    for prefix in CLASSIC {
        if let Some(after) = rest.strip_prefix(prefix) {
            let len = run_len(after, |c| c.is_ascii_alphanumeric());
            if len >= MIN_TOKEN_BODY {
                return Some(start + prefix.len() + len);
            }
            return None;
        }
    }
    None
}

/// Counts the leading run of bytes satisfying `pred`.
fn run_len(s: &str, pred: impl Fn(char) -> bool) -> usize {
    s.bytes().take_while(|&b| pred(b as char)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(prefix: &str) -> String {
        format!("{prefix}{}", "A".repeat(40))
    }

    #[test]
    fn scrubs_classic_pat() {
        let tok = long("ghp_");
        let line = format!("error using token {tok} now");
        let scrubbed = scrub(&line);
        assert!(!scrubbed.contains(&tok));
        assert!(scrubbed.contains(SCRUB_MARKER));
        assert!(scrubbed.starts_with("error using token "));
    }

    #[test]
    fn scrubs_fine_grained_pat() {
        let tok = long("github_pat_");
        let scrubbed = scrub(&tok);
        assert_eq!(scrubbed, SCRUB_MARKER);
    }

    #[test]
    fn leaves_short_lookalikes_untouched() {
        // "ghp_test" is too short to be a real token.
        let line = "ghp_test and gh references";
        assert_eq!(scrub(line), line);
    }

    #[test]
    fn scrubs_multiple_tokens() {
        let a = long("ghp_");
        let b = long("github_pat_");
        let line = format!("{a} {b}");
        let scrubbed = scrub(&line);
        assert!(!scrubbed.contains(&a));
        assert!(!scrubbed.contains(&b));
        assert_eq!(scrubbed, format!("{SCRUB_MARKER} {SCRUB_MARKER}"));
    }

    #[test]
    fn preserves_non_token_text() {
        let line = "no secrets here, just prose about github";
        assert_eq!(scrub(line), line);
    }
}
