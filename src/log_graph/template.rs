//! `template-v1` log-message normalization (issues #319 / #320).
//!
//! Deterministically replaces variable spans (timestamps, UUIDs, IP addresses,
//! durations, hex runs, paths, and bare numbers) with closed placeholder
//! tokens so that occurrences of the same error differing only in those spans
//! collapse to one fingerprint. No regex dependency: a single left-to-right
//! scan tries the matchers below in priority order at each token position, and
//! the most specific match wins.
//!
//! Placeholder order (specifics before `<NUM>` so they win):
//! `<TS>` `<UUID>` `<IP>` `<DUR>` `<HEX>` `<PATH>` `<NUM>`.

/// Applies the `template-v1` normalization to a log message.
///
/// The scan is byte-stable and allocation-bounded; the same input always yields
/// the same output. Multi-line input (a panic + backtrace) is normalized
/// line-agnostically — newlines are copied through.
#[must_use]
pub fn normalize_template_v1(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some((token, end)) = match_any(&chars, i) {
            out.push_str(token);
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Tries each matcher at position `i` in priority order.
fn match_any(chars: &[char], i: usize) -> Option<(&'static str, usize)> {
    if let Some(end) = match_timestamp(chars, i) {
        return Some(("<TS>", end));
    }
    if let Some(end) = match_uuid(chars, i) {
        return Some(("<UUID>", end));
    }
    if let Some(end) = match_ip(chars, i) {
        return Some(("<IP>", end));
    }
    if let Some(end) = match_duration(chars, i) {
        return Some(("<DUR>", end));
    }
    if let Some(end) = match_hex(chars, i) {
        return Some(("<HEX>", end));
    }
    if let Some(end) = match_path(chars, i) {
        return Some(("<PATH>", end));
    }
    if let Some(end) = match_num(chars, i) {
        return Some(("<NUM>", end));
    }
    None
}

// ── boundary helpers ─────────────────────────────────────────────────────────

const fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A token may start at `i` only when the previous char is not part of a word,
/// so we never rewrite the interior of an identifier.
fn token_start(chars: &[char], i: usize) -> bool {
    i == 0 || !is_word(chars[i - 1])
}

/// A matched span ending at `end` must not be immediately followed by a word
/// char, so `deadbeef` inside `deadbeefx` is not treated as a hex run.
fn boundary_after(chars: &[char], end: usize) -> bool {
    end >= chars.len() || !is_word(chars[end])
}

/// Consumes exactly `n` ASCII digits starting at `i`; returns the end index.
fn exact_digits(chars: &[char], i: usize, n: usize) -> Option<usize> {
    if i + n > chars.len() {
        return None;
    }
    if chars[i..i + n].iter().all(char::is_ascii_digit) {
        Some(i + n)
    } else {
        None
    }
}

/// Consumes one literal char `c` at `i`.
fn lit(chars: &[char], i: usize, c: char) -> Option<usize> {
    if chars.get(i) == Some(&c) {
        Some(i + 1)
    } else {
        None
    }
}

/// Consumes one-or-more ASCII digits.
fn some_digits(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j > i { Some(j) } else { None }
}

// ── matchers ─────────────────────────────────────────────────────────────────

/// `<TS>` — ISO/RFC3339, `YYYY-MM-DD[ T]HH:MM:SS[.f][Z|±HH[:]MM]`, a bare
/// `HH:MM:SS[.f]` clock, or a syslog `Mon DD HH:MM:SS`.
fn match_timestamp(chars: &[char], i: usize) -> Option<usize> {
    if !token_start(chars, i) {
        return None;
    }
    if let Some(end) = match_iso(chars, i) {
        return Some(end);
    }
    if let Some(end) = match_syslog(chars, i) {
        return Some(end);
    }
    match_clock(chars, i)
}

fn match_iso(chars: &[char], i: usize) -> Option<usize> {
    let mut j = exact_digits(chars, i, 4)?; // year
    j = lit(chars, j, '-')?;
    j = exact_digits(chars, j, 2)?; // month
    j = lit(chars, j, '-')?;
    j = exact_digits(chars, j, 2)?; // day
    match chars.get(j) {
        Some('T' | ' ') => j += 1,
        _ => return None,
    }
    j = exact_digits(chars, j, 2)?; // hour
    j = lit(chars, j, ':')?;
    j = exact_digits(chars, j, 2)?; // minute
    j = lit(chars, j, ':')?;
    j = exact_digits(chars, j, 2)?; // second
    if chars.get(j) == Some(&'.') {
        j = some_digits(chars, j + 1)?;
    }
    match chars.get(j) {
        Some('Z') => j += 1,
        Some('+' | '-') => {
            j = exact_digits(chars, j + 1, 2)?;
            if chars.get(j) == Some(&':') {
                j += 1;
            }
            j = exact_digits(chars, j, 2)?;
        }
        _ => {}
    }
    Some(j)
}

fn match_clock(chars: &[char], i: usize) -> Option<usize> {
    let mut j = exact_digits(chars, i, 2)?;
    j = lit(chars, j, ':')?;
    j = exact_digits(chars, j, 2)?;
    j = lit(chars, j, ':')?;
    j = exact_digits(chars, j, 2)?;
    if chars.get(j) == Some(&'.') {
        j = some_digits(chars, j + 1)?;
    }
    if boundary_after(chars, j) {
        Some(j)
    } else {
        None
    }
}

fn match_syslog(chars: &[char], i: usize) -> Option<usize> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    if i + 3 > chars.len() {
        return None;
    }
    let abbrev: String = chars[i..i + 3].iter().collect();
    if !MONTHS.contains(&abbrev.as_str()) {
        return None;
    }
    let mut j = i + 3;
    j = lit(chars, j, ' ')?;
    while chars.get(j) == Some(&' ') {
        j += 1;
    }
    // 1-2 digit day
    let day_start = j;
    while j < chars.len() && chars[j].is_ascii_digit() && j - day_start < 2 {
        j += 1;
    }
    if j == day_start {
        return None;
    }
    j = lit(chars, j, ' ')?;
    // HH:MM:SS
    j = exact_digits(chars, j, 2)?;
    j = lit(chars, j, ':')?;
    j = exact_digits(chars, j, 2)?;
    j = lit(chars, j, ':')?;
    j = exact_digits(chars, j, 2)?;
    Some(j)
}

/// `<UUID>` — canonical 8-4-4-4-12 hex form.
fn match_uuid(chars: &[char], i: usize) -> Option<usize> {
    if !token_start(chars, i) {
        return None;
    }
    let groups = [8, 4, 4, 4, 12];
    let mut j = i;
    for (idx, len) in groups.iter().enumerate() {
        if j + len > chars.len() || !chars[j..j + len].iter().all(char::is_ascii_hexdigit) {
            return None;
        }
        j += len;
        if idx < groups.len() - 1 {
            j = lit(chars, j, '-')?;
        }
    }
    if boundary_after(chars, j) {
        Some(j)
    } else {
        None
    }
}

/// `<IP>` — IPv4 (with optional `:port`) or a conservative IPv6 form.
fn match_ip(chars: &[char], i: usize) -> Option<usize> {
    if !token_start(chars, i) {
        return None;
    }
    if let Some(end) = match_ipv4(chars, i) {
        return Some(end);
    }
    match_ipv6(chars, i)
}

fn match_ipv4(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    for octet in 0..4 {
        let start = j;
        while j < chars.len() && chars[j].is_ascii_digit() && j - start < 3 {
            j += 1;
        }
        if j == start {
            return None;
        }
        if octet < 3 {
            j = lit(chars, j, '.')?;
        }
    }
    // optional :port
    if chars.get(j) == Some(&':') {
        let port = some_digits(chars, j + 1)?;
        j = port;
    }
    if boundary_after(chars, j) {
        Some(j)
    } else {
        None
    }
}

fn match_ipv6(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    let mut colon_count = 0;
    let mut double_colon = false;
    while j < chars.len() && (chars[j].is_ascii_hexdigit() || chars[j] == ':') {
        if chars[j] == ':' {
            colon_count += 1;
            if chars.get(j + 1) == Some(&':') {
                double_colon = true;
            }
        }
        j += 1;
    }
    // Require a genuine IPv6 shape: either compressed `::` or a full 7-colon run.
    if j > i && (double_colon || colon_count >= 4) && boundary_after(chars, j) {
        Some(j)
    } else {
        None
    }
}

/// `<DUR>` — one or more `number+unit` segments (`1.5s`, `250ms`, `1m30s`).
fn match_duration(chars: &[char], i: usize) -> Option<usize> {
    if !token_start(chars, i) {
        return None;
    }
    let mut j = i;
    let mut segments = 0;
    loop {
        let Some(after_num) = match_decimal(chars, j) else {
            break;
        };
        let Some(after_unit) = match_unit(chars, after_num) else {
            break;
        };
        j = after_unit;
        segments += 1;
    }
    if segments >= 1 && boundary_after(chars, j) {
        Some(j)
    } else {
        None
    }
}

/// Consumes `digits[.digits]`.
fn match_decimal(chars: &[char], i: usize) -> Option<usize> {
    let mut j = some_digits(chars, i)?;
    if chars.get(j) == Some(&'.')
        && let Some(after) = some_digits(chars, j + 1)
    {
        j = after;
    }
    Some(j)
}

/// Consumes a duration unit: `ns`, `ms`, `us`, `µs`, `s`, `m`, or `h`.
fn match_unit(chars: &[char], i: usize) -> Option<usize> {
    let two: String = chars
        .get(i..i + 2)
        .map(|s| s.iter().collect())
        .unwrap_or_default();
    if matches!(two.as_str(), "ns" | "ms" | "us" | "µs") {
        return Some(i + 2);
    }
    match chars.get(i) {
        Some('s' | 'm' | 'h') => Some(i + 1),
        _ => None,
    }
}

/// `<HEX>` — `0x…` hex literal or a bare 6+-digit hex run.
fn match_hex(chars: &[char], i: usize) -> Option<usize> {
    // 0x-prefixed.
    if token_start(chars, i)
        && chars.get(i) == Some(&'0')
        && matches!(chars.get(i + 1), Some('x' | 'X'))
    {
        let mut j = i + 2;
        let start = j;
        while j < chars.len() && chars[j].is_ascii_hexdigit() {
            j += 1;
        }
        if j > start && boundary_after(chars, j) {
            return Some(j);
        }
    }
    // Bare 6+ hex run at a token boundary.
    if !token_start(chars, i) {
        return None;
    }
    let mut j = i;
    while j < chars.len() && chars[j].is_ascii_hexdigit() {
        j += 1;
    }
    if j - i >= 6 && boundary_after(chars, j) {
        Some(j)
    } else {
        None
    }
}

/// `<PATH>` — a Unix/Windows path containing at least one separator, plus an
/// optional trailing `:line[:col]`.
fn match_path(chars: &[char], i: usize) -> Option<usize> {
    // Windows drive path: `C:\...`.
    if chars.get(i).is_some_and(char::is_ascii_alphabetic)
        && chars.get(i + 1) == Some(&':')
        && chars.get(i + 2) == Some(&'\\')
    {
        let mut j = i + 2;
        while j < chars.len() && is_path_char(chars[j]) {
            j += 1;
        }
        return Some(j);
    }

    let prev_ok = i == 0 || (!is_word(chars[i - 1]) && chars[i - 1] != '/' && chars[i - 1] != '.');
    if !prev_ok {
        return None;
    }
    let mut j = i;
    let mut saw_slash = false;
    while j < chars.len() && is_path_segment_char(chars[j]) {
        if chars[j] == '/' {
            saw_slash = true;
        }
        j += 1;
    }
    if !saw_slash || j == i {
        return None;
    }
    // Optional `:line[:col]`.
    if chars.get(j) == Some(&':')
        && let Some(after) = some_digits(chars, j + 1)
    {
        j = after;
        if chars.get(j) == Some(&':')
            && let Some(after_col) = some_digits(chars, j + 1)
        {
            j = after_col;
        }
    }
    Some(j)
}

const fn is_path_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_')
}

const fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '\\' | '/' | '.' | '-' | '_')
}

/// `<NUM>` — a bare integer or decimal at a token boundary.
fn match_num(chars: &[char], i: usize) -> Option<usize> {
    if !token_start(chars, i) || !chars[i].is_ascii_digit() {
        return None;
    }
    let j = match_decimal(chars, i)?;
    if boundary_after(chars, j) {
        Some(j)
    } else {
        None
    }
}
