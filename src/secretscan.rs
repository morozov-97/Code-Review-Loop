//! #122: the diff (and any --requirements/--conventions content) is sent verbatim to an
//! external LLM provider with no redaction. This is a best-effort, local, pattern-based scan
//! that runs before any LLM call, so a secret visible anywhere in what's about to be sent
//! doesn't leave the machine silently. It is not a substitute for a real secret scanner
//! (gitleaks/trufflehog) in CI — see the README caveat this links to.
//!
//! #155: scans every line inside a diff hunk body — added, removed, *and* context — not just
//! added ones. The transmission boundary is "the whole diff text" (see promptctx::shared_context,
//! which fences `input.diff` in full), not "the added lines" — a PR that *removes* a leaked
//! secret still has that secret's line sitting in the diff text sent to the LLM, so scanning
//! only `+` lines missed it.

#[derive(Debug)]
pub(crate) struct SecretHit {
    pub(crate) file: String,
    pub(crate) pattern: &'static str,
    /// First/last 4 chars kept, the rest masked — enough to eyeball which secret it is without
    /// putting the full value in a terminal/log/report.
    pub(crate) redacted: String,
}

/// #137: previously matched `"+++ "` against every line unconditionally, so an *added* line
/// whose own content started with `+++`/`++` (e.g. reviewing a diff/patch file, or content that
/// legitimately starts with `+`) was either mistaken for a file-header line (and dropped) or
/// explicitly skipped by a follow-up `starts_with('+')` check — either way, never scanned. Mirrors
/// `input::parse_diff_stats`'s `in_hunk_body` tracking: `"+++ "` only means "file header" in the
/// diff header section (before the first `@@`); once inside a hunk body, every `+`-prefixed line
/// is added content, full stop, regardless of what character comes right after that `+`.
pub(crate) fn scan(diff: &str) -> Vec<SecretHit> {
    let mut hits = Vec::new();
    let mut current_file = String::from("(unknown file)");
    let mut in_hunk_body = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk_body = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk_body = true;
            continue;
        }
        if !in_hunk_body {
            if let Some(rest) = line.strip_prefix("+++ ") {
                let path = rest.strip_prefix("b/").unwrap_or(rest);
                if path != "/dev/null" {
                    current_file = path.to_string();
                }
            }
            continue;
        }
        // #155: every hunk-body line is scanned regardless of its leading marker (+/-/space) —
        // strip exactly one marker char if present so the pattern matchers see the same content
        // whether it's an added, removed, or context line.
        let content = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix('-'))
            .or_else(|| line.strip_prefix(' '))
            .unwrap_or(line);
        for (pattern, value) in find_secrets(content) {
            hits.push(SecretHit {
                file: current_file.clone(),
                pattern,
                redacted: redact(value),
            });
        }
    }
    hits
}

/// #137: requirements/conventions content is sent to the LLM verbatim just like the diff (see
/// `promptctx::shared_context`), but wasn't scanned at all — only `scan()` (diff-shaped input)
/// was wired up. This is the same pattern-matching core applied to plain text: every non-empty
/// line is scanned directly, with no diff marker to strip.
pub(crate) fn scan_text(label: &str, text: &str) -> Vec<SecretHit> {
    let mut hits = Vec::new();
    for line in text.lines() {
        for (pattern, value) in find_secrets(line) {
            hits.push(SecretHit {
                file: label.to_string(),
                pattern,
                redacted: redact(value),
            });
        }
    }
    hits
}

/// #138: operates on chars, not bytes — the previous byte-index slicing (`&value[..4]`) panicked
/// whenever a multi-byte UTF-8 char straddled the byte-4/byte-(len-4) boundary (e.g. a Korean/
/// Japanese/Cyrillic secret value matched by find_env_style_secret).
fn redact(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let prefix: String = chars[..4].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}{}{suffix}", "*".repeat(chars.len() - 8))
}

/// Returns every (pattern name, matched value) found in one added line. Hand-rolled instead of
/// pulling in the `regex` crate — the patterns here are simple prefix/shape checks, and #128
/// already flags dependency footprint as worth minimizing.
fn find_secrets(line: &str) -> Vec<(&'static str, &str)> {
    let mut found = Vec::new();

    if let Some(m) = find_token(line, "AKIA", 16, is_upper_alnum) {
        found.push(("AWS access key ID", m));
    }
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
        if let Some(m) = find_token(line, prefix, 20, is_alnum) {
            found.push(("GitHub token", m));
        }
    }
    for prefix in ["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"] {
        if let Some(m) = find_token(line, prefix, 10, |c| c.is_ascii_digit() || c == '-') {
            found.push(("Slack token", m));
        }
    }
    // Real false positive: code that *processes* PEM marker strings (e.g. a regex stripping
    // `-----BEGIN PRIVATE KEY-----`/`-----END PRIVATE KEY-----` from an env-supplied key before
    // use) mentions both marker strings as text, not as an actual embedded key. A real PEM
    // block's BEGIN and END lines are never the same line — the base64 body sits between them
    // across multiple lines — so a single line naming both is a reference to the markers, not a
    // literal key.
    if line.contains("-----BEGIN")
        && line.contains("PRIVATE KEY-----")
        && !line.contains("-----END")
    {
        found.push(("PEM private key block", line.trim()));
    }
    if let Some(m) = find_jwt(line) {
        found.push(("JWT-shaped token", m));
    }
    if let Some(m) = find_env_style_secret(line) {
        found.push(("assigned secret-like value", m));
    }
    // PII: deliberately narrow in scope. Emails/phone numbers/national IDs are far too common as
    // *legitimate* content in ordinary source (test fixtures, contact info, config defaults) to
    // flag on shape alone without drowning real hits in noise — the same lesson already learned
    // the hard way tuning the credential patterns above. A Luhn-valid 13-19 digit run is a much
    // stronger signal: it's specifically what a real payment card number looks like, and an
    // arbitrary digit run this long passing Luhn by chance is rare (~1 in 10). Broader PII
    // detection (emails, phone numbers, national IDs) needs more context than a line-by-line
    // pattern scan can give and isn't attempted here.
    if let Some(m) = find_luhn_valid_card_number(line) {
        found.push(("possible payment card number (Luhn-valid)", m));
    }

    found
}

/// Digits with interior spaces/dashes allowed (how card numbers are usually written:
/// `4111 1111 1111 1111` / `4111-1111-1111-1111`), matching the run in whichever length range
/// (13-19 digits) checks out via Luhn.
fn find_luhn_valid_card_number(line: &str) -> Option<&str> {
    let mut run_start: Option<usize> = None;
    let mut run_end = 0usize;
    for (i, c) in line.char_indices() {
        if c.is_ascii_digit() || c == ' ' || c == '-' {
            if run_start.is_none() {
                run_start = Some(i);
            }
            run_end = i + c.len_utf8();
        } else if let Some(s) = run_start.take() {
            if is_luhn_valid_card_run(&line[s..run_end]) {
                return Some(&line[s..run_end]);
            }
        }
    }
    if let Some(s) = run_start {
        if is_luhn_valid_card_run(&line[s..run_end]) {
            return Some(&line[s..run_end]);
        }
    }
    None
}

fn is_luhn_valid_card_run(run: &str) -> bool {
    let digits: String = run.chars().filter(|c| c.is_ascii_digit()).collect();
    (13..=19).contains(&digits.len()) && luhn_checksum_valid(&digits)
}

fn luhn_checksum_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    for (i, c) in digits.chars().rev().enumerate() {
        let mut d = c
            .to_digit(10)
            .expect("digits is filtered to ASCII digits only");
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    sum % 10 == 0
}

/// Finds `prefix` in `line`, then greedily consumes characters matching `is_body` right after
/// it; returns the whole prefix+body span if the body reached at least `min_body_len`.
fn find_token<'a>(
    line: &'a str,
    prefix: &str,
    min_body_len: usize,
    is_body: fn(char) -> bool,
) -> Option<&'a str> {
    let start = line.find(prefix)?;
    let body_start = start + prefix.len();
    let body = &line[body_start..];
    let body_len = body.chars().take_while(|&c| is_body(c)).count();
    if body_len < min_body_len {
        return None;
    }
    let end_byte = body_start
        + body
            .char_indices()
            .nth(body_len)
            .map_or(body.len(), |(i, _)| i);
    Some(&line[start..end_byte])
}

fn is_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn is_upper_alnum(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

/// `eyJ` is the base64 encoding of `{"` — the near-universal start of a JWT header — followed
/// by two more base64url segments separated by dots.
fn find_jwt(line: &str) -> Option<&str> {
    let start = line.find("eyJ")?;
    let rest = &line[start..];
    let is_b64url = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let seg1_len = rest.chars().take_while(|&c| is_b64url(c)).count();
    if seg1_len < 10 || rest.as_bytes().get(seg1_len) != Some(&b'.') {
        return None;
    }
    let after_dot1 = &rest[seg1_len + 1..];
    let seg2_len = after_dot1.chars().take_while(|&c| is_b64url(c)).count();
    if seg2_len < 10 || after_dot1.as_bytes().get(seg2_len) != Some(&b'.') {
        return None;
    }
    let after_dot2 = &after_dot1[seg2_len + 1..];
    let seg3_len = after_dot2.chars().take_while(|&c| is_b64url(c)).count();
    if seg3_len < 5 {
        return None;
    }
    let total = seg1_len + 1 + seg2_len + 1 + seg3_len;
    Some(&rest[..total])
}

const SECRET_KEY_MARKERS: [&str; 8] = [
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "PRIVATE_KEY",
    "TOKEN",
];

/// #181: key names that are TOKEN-flavored but name a plain count/size, not a credential — a
/// bare `"TOKEN"` marker with only word-boundary checking still matches these exactly (e.g.
/// `MAX_TOKENS` has real boundaries — `_` before, end-of-string after), so they need an explicit
/// carve-out. Checked as an exact match on the narrowed `key` (see below), not a substring, so
/// this stays a narrow exception rather than a way to defeat the scanner — `TOKEN_SECRET` still
/// trips the generic marker check since it isn't in this list.
const BENIGN_TOKEN_KEY_NAMES: [&str; 6] = [
    "MAX_TOKENS",
    "NUM_TOKENS",
    "TOKEN_COUNT",
    "TOKEN_LIMIT",
    "TOKENIZER",
    "TOKENS",
];

const PLACEHOLDER_VALUES: [&str; 8] = [
    "xxx",
    "changeme",
    "change_me",
    "your_key_here",
    "todo",
    "fixme",
    "example",
    "placeholder",
];

/// #181: true if `needle` appears in `haystack` as a whole "word" — not immediately preceded or
/// followed by another alphanumeric character. Without this, a marker like `"TOKEN"` matched
/// mid-word inside an unrelated identifier via plain substring search — `"TOKENIZER"` contains
/// `"TOKEN"` followed directly by `"IZER"` (not a real boundary), so it isn't actually
/// token/credential-flavored just because the letters happen to line up.
fn contains_marker_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(rel) = haystack.get(start..).and_then(|h| h.find(needle)) {
        let idx = start + rel;
        let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let after = idx + needle.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

/// True when the `=`/`:` at `idx` is actually part of a comparison/relational operator
/// (`==`, `!=`, `<=`, `>=`) rather than an assignment — found via a real false positive on
/// `if (token == null) {`, where the first `=` of `==` was mistaken for an assignment separator
/// and everything after the *second* `=` (`" null) {"`) was captured as "the value".
fn is_comparison_operator_at(trimmed: &str, idx: usize) -> bool {
    let bytes = trimmed.as_bytes();
    if bytes.get(idx + 1) == Some(&b'=') {
        return true; // `==`
    }
    if idx > 0 && matches!(bytes[idx - 1], b'!' | b'<' | b'>' | b'=') {
        return true; // `!=`, `<=`, `>=`, or a second `=` in `==`
    }
    false
}

/// True when `value` is nothing but a dotted identifier/property-access chain (e.g.
/// `AppConfig.supabaseAnonKey`) — found via a real false positive where a *reference* to a
/// config value, not the value itself, sat on the right of `=` and was indistinguishable from a
/// literal secret by length/placeholder checks alone. A real credential is an opaque token, not
/// a chain of identifiers joined by `.` — this doesn't cover every non-literal expression (e.g.
/// a function call), just the shape that actually produced a false positive.
fn is_dotted_identifier_chain(value: &str) -> bool {
    let stripped = value.trim_end_matches([';', ')', '{', '}']);
    !stripped.is_empty()
        && stripped.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        && stripped.contains('.')
        && stripped
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

/// True when `value` contains whitespace or parentheses, or starts with `++`/`--` — found via
/// two real false positives: `final token = (await freshToken()) ?? supabaseKey;` (the whole
/// right-hand-side expression, a function call plus a `??` fallback, captured as "the value"),
/// and `final token = ++_speechToken;` (a bare pre/post-increment expression on a private
/// field). A real credential is a single opaque token; it never contains a space or a paren
/// (it wouldn't work as a token in code if it did), and never starts with a doubled `+`/`-` —
/// unlike a single leading `-`/`+` (plausible in some token formats, e.g. base64's `+`), a
/// *doubled* one at the very start is specifically the increment/decrement operator, not
/// something any real credential format uses.
fn looks_like_a_code_expression(value: &str) -> bool {
    value.starts_with("++")
        || value.starts_with("--")
        || value
            .chars()
            .any(|c| c.is_whitespace() || c == '(' || c == ')')
}

/// True when `value` is a bare identifier with no digits at all — e.g. `supabaseKey`,
/// `freshToken`. Found via a real false positive on `'apikey': supabaseKey,`, where a plain
/// variable reference (not its value) sat on the right of a `KEY`-flavored key. Deliberately
/// narrow: only rejects when there isn't a single digit anywhere in the value, since real
/// credential formats (AWS/GitHub/Slack/JWT) are covered by their own dedicated patterns
/// elsewhere in this file and virtually always contain digits — an *entirely* alphabetic value
/// reads far more like an English identifier than a generated secret. Disclosed tradeoff: a
/// real secret that happens to be purely alphabetic (no digits at all) would be missed by this
/// specific check — accepted as a real, bounded false-negative risk rather than leaving the
/// original false positive unfixed (see #185).
fn is_pure_alphabetic_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
        && value.chars().any(|c| c.is_ascii_alphabetic())
}

/// Catches `.env`-style `KEY=value` (or `KEY: "value"` in YAML/JSON-ish config) lines where the
/// key name looks secret-flavored and the value isn't an obvious placeholder or an
/// interpolation reference like `${VAR}`/`$VAR`.
fn find_env_style_secret(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let sep_idx = trimmed
        .find(['=', ':'])
        .filter(|&i| i > 0 && i < trimmed.len() - 1)
        .filter(|&i| !is_comparison_operator_at(trimmed, i))?;
    let raw_key = trimmed[..sep_idx].trim();
    // #181: only the identifier immediately before the separator is "the key" — a line like
    // `model, max_tokens: value` (a destructured/positional parameter list, not a single
    // assignment) previously treated the whole `"model, max_tokens"` prefix as one key, which
    // is neither a real identifier nor what a human would call "the key name" here.
    let key = raw_key
        .rsplit([',', ' ', '\t'])
        .find(|s| !s.is_empty())
        .unwrap_or(raw_key);
    let key_upper = key.to_ascii_uppercase();
    if BENIGN_TOKEN_KEY_NAMES.contains(&key_upper.as_str()) {
        return None;
    }
    if !SECRET_KEY_MARKERS
        .iter()
        .any(|m| contains_marker_word(&key_upper, m))
    {
        return None;
    }
    // `;` added alongside the quote/comma set — without it, a real quoted secret with a
    // trailing statement semicolon (`const apiKey = "realSecret123";`, ubiquitous in Dart/JS/TS/
    // Java/C/Rust/Go source) only had its closing quote stripped, not the semicolon after it,
    // since trim_matches stops at the first non-matching char from each end and `;` didn't
    // match — leaving a value like `realSecret123";` that a later code-expression check could
    // then wrongly reject as non-literal. Stripping `;` in the same pass as quotes/commas fixes
    // both directions: a real quoted secret ends up clean, and a bare code statement's trailing
    // `;` no longer masks whatever comes before it from the checks below.
    let value = trimmed[sep_idx + 1..]
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == ',' || c == ';');
    if value.len() < 8 {
        return None;
    }
    if value.starts_with('$') || value.starts_with('{') || value.starts_with('<') {
        return None; // interpolated/templated, not a literal secret
    }
    if is_dotted_identifier_chain(value) {
        return None; // a reference to another value (e.g. `AppConfig.supabaseAnonKey`), not a literal
    }
    if looks_like_a_code_expression(value) {
        return None; // e.g. `(await freshToken()) ?? supabaseKey;` — an expression, not a literal
    }
    if is_pure_alphabetic_identifier(value) {
        return None; // e.g. `supabaseKey` — a bare variable reference, not a literal (see #185)
    }
    let value_lower = value.to_ascii_lowercase();
    if PLACEHOLDER_VALUES.iter().any(|p| value_lower.contains(p)) {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_aws_access_key_in_an_added_line() {
        let diff = "diff --git a/config.py b/config.py\n\
                     +++ b/config.py\n\
                     @@ -1 +1 @@\n\
                     +AWS_KEY = \"AKIAABCDEFGHIJKLMNOP\"\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "AWS access key ID"));
        assert_eq!(hits[0].file, "config.py");
    }

    #[test]
    fn scan_finds_github_token() {
        let diff = "+++ b/notes.md\n@@ -1 +1 @@\n+token: ghp_1234567890abcdEFGHijklMNOP\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "GitHub token"));
    }

    #[test]
    fn scan_finds_pem_private_key_header() {
        let diff = "+++ b/id_rsa\n@@ -1 +1 @@\n+-----BEGIN RSA PRIVATE KEY-----\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "PEM private key block"));
    }

    #[test]
    fn scan_does_not_flag_code_that_processes_pem_marker_strings_as_text() {
        // Real false positive: a regex stripping PEM headers from an env-supplied key before
        // use mentions both marker strings on one line — a real embedded PEM block's BEGIN and
        // END lines are never the same line (the base64 body sits between them), so this is
        // code referencing the markers as text, not a literal key.
        let diff = "+++ b/index.ts\n@@ -1 +1 @@\n\
                     +      .replace(/-----BEGIN PRIVATE KEY-----|-----END PRIVATE KEY-----|\\n/g, '')\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a line mentioning both markers is processing them as text, not a literal key: {hits:?}"
        );
    }

    #[test]
    fn scan_finds_jwt_shaped_token() {
        let diff = "+++ b/auth.js\n@@ -1 +1 @@\n\
                     +const t = \"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U\";\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "JWT-shaped token"));
    }

    #[test]
    fn scan_finds_env_style_secret_but_skips_placeholders_and_interpolation() {
        let diff = "+++ b/.env\n@@ -1,3 +1,3 @@\n\
                     +DB_PASSWORD=hunter2_but_longer_than_eight\n\
                     +API_KEY=changeme\n\
                     +API_TOKEN=${SECRET_FROM_VAULT}\n";
        let hits = scan(diff);
        let env_hits: Vec<_> = hits
            .iter()
            .filter(|h| h.pattern == "assigned secret-like value")
            .collect();
        assert_eq!(env_hits.len(), 1);
        assert!(env_hits[0].redacted.starts_with("hunt"));
    }

    // --- #181: bare "TOKEN" marker false-positiving on non-secret identifiers ---

    #[test]
    fn scan_does_not_flag_a_comparison_against_null_as_a_secret_assignment() {
        // Real false positive: the first `=` of `==` was mistaken for an assignment separator,
        // capturing "null) {" as if it were a credential value for a "token"-named identifier.
        let diff = "+++ b/lib/x.dart\n@@ -1 +1 @@\n\
                     +    if (token == null) {\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a `== null` comparison is not a secret assignment: {hits:?}"
        );
    }

    #[test]
    fn scan_does_not_flag_a_dotted_property_reference_as_a_secret_value() {
        // Real false positive: `AppConfig.supabaseAnonKey` is a *reference* to a config value,
        // not a literal credential — indistinguishable from a real secret by length/placeholder
        // checks alone, since it's a plausible-length non-placeholder string.
        let diff = "+++ b/lib/x.dart\n@@ -1 +1 @@\n\
                     +    const supabaseKey = AppConfig.supabaseAnonKey;\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a dotted identifier chain is a reference, not a literal secret: {hits:?}"
        );
    }

    #[test]
    fn scan_still_flags_a_real_looking_api_key_value_assigned_to_a_key_named_variable() {
        // Guards against `is_dotted_identifier_chain` overreaching — a real credential-shaped
        // value must still be caught even when assigned to a similarly-named variable.
        let diff = "+++ b/lib/x.dart\n@@ -1 +1 @@\n\
                     +    const supabaseKey = \"AKIAABCDEFGHIJKLMNOP\";\n";
        let hits = scan(diff);
        assert!(
            !hits.is_empty(),
            "a real credential-shaped literal must still be flagged"
        );
    }

    #[test]
    fn scan_does_not_flag_a_code_expression_captured_after_a_token_named_variable() {
        // Real false positive (#185): the whole right-hand-side expression — a function call
        // plus a `??` fallback — was captured as if it were a literal secret.
        let diff = "+++ b/lib/x.dart\n@@ -1 +1 @@\n\
                     +    final token = (await freshToken()) ?? supabaseKey;\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a code expression is not a literal secret value: {hits:?}"
        );
    }

    #[test]
    fn scan_does_not_flag_a_bare_increment_expression_assigned_to_a_token_named_variable() {
        // Real false positive: `++_speechToken` is a pre-increment expression on a private
        // field, not a literal — no whitespace/parens, so this needed its own check separate
        // from the code-expression-with-parens case above.
        let diff = "+++ b/lib/x.dart\n@@ -1 +1 @@\n\
                     +    final token = ++_speechToken;\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a bare increment expression is not a literal secret value: {hits:?}"
        );
    }

    #[test]
    fn scan_still_flags_a_real_secret_that_ends_with_a_quoted_semicolon() {
        // Guards against the `;` fix above overreaching in the other direction: a real,
        // generic (non-prefix-recognized) secret literal followed by a statement semicolon —
        // ubiquitous in Dart/JS/TS/Java/C/Rust/Go source — must still be caught. Before adding
        // `;` to the value's trim_matches set, trim_matches stopped at the first non-matching
        // trailing char (`;` didn't match), leaving `realSecretValue123";` — this itself isn't
        // what regressed, but confirms the value extraction still works correctly end-to-end.
        let diff = "+++ b/lib/x.dart\n@@ -1 +1 @@\n\
                     +    const API_TOKEN = \"realSecretValue123\";\n";
        let hits = scan(diff);
        assert!(
            !hits.is_empty(),
            "a real quoted secret followed by a semicolon must still be flagged: {hits:?}"
        );
    }

    #[test]
    fn scan_flags_a_luhn_valid_card_number() {
        let diff = "+++ b/fixtures/customer.json\n@@ -1 +1 @@\n\
                     +    \"card_number\": \"4532015112830366\",\n";
        let hits = scan(diff);
        assert!(
            hits.iter().any(|h| h.pattern.contains("card number")),
            "a Luhn-valid 16-digit run must be flagged as a possible card number: {hits:?}"
        );
    }

    #[test]
    fn scan_flags_a_luhn_valid_card_number_written_with_spaces() {
        let diff = "+++ b/fixtures/customer.json\n@@ -1 +1 @@\n\
                     +    cardNumber = \"4111 1111 1111 1111\"\n";
        let hits = scan(diff);
        assert!(
            hits.iter().any(|h| h.pattern.contains("card number")),
            "a space-grouped Luhn-valid card number must still be flagged: {hits:?}"
        );
    }

    #[test]
    fn scan_does_not_flag_an_arbitrary_16_digit_number_that_fails_luhn() {
        // Ordinary numeric ids/constants of card-like length are common in real code and must
        // not be flagged just for being 16 digits — only a Luhn-valid run is a real signal.
        let diff = "+++ b/src/constants.rs\n@@ -1 +1 @@\n\
                     +    const MAGIC_ID: u64 = 1234567890123456;\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a 16-digit number that fails the Luhn check must not be flagged: {hits:?}"
        );
    }

    #[test]
    fn scan_does_not_flag_an_ordinary_short_number_even_if_it_would_pass_luhn() {
        // Luhn validity alone isn't enough at short lengths -- phone numbers, short ids, etc.
        // are common and some fraction will happen to pass Luhn by chance; the length gate
        // (13-19 digits) is what keeps that rare instead of routine.
        let diff = "+++ b/src/constants.rs\n@@ -1 +1 @@\n\
                     +    const PORT: u16 = 8080;\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a short number must not be flagged as a card number: {hits:?}"
        );
    }

    #[test]
    fn scan_does_not_flag_a_bare_identifier_with_no_digits_assigned_to_a_key_named_field() {
        // Real false positive (#185): `supabaseKey` is a plain variable reference, not the
        // credential value itself.
        let diff = "+++ b/lib/x.dart\n@@ -1 +1 @@\n\
                     +          'apikey': supabaseKey,\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "a bare identifier with no digits reads as a variable reference, not a secret: {hits:?}"
        );
    }

    #[test]
    fn scan_still_flags_a_bare_value_that_contains_a_digit() {
        // Guards against `is_pure_alphabetic_identifier` overreaching — the presence of even one
        // digit should be enough to still treat a bare env-style value as a real credential
        // candidate, since that's the disclosed boundary the function's own doc comment draws.
        let diff = "+++ b/.env\n@@ -1 +1 @@\n+API_KEY=abcXYZ123longenough\n";
        let hits = scan(diff);
        assert!(
            !hits.is_empty(),
            "a bare value containing a digit must still be flagged: {hits:?}"
        );
    }

    #[test]
    fn scan_does_not_flag_an_ordinary_max_tokens_api_parameter() {
        let diff = "+++ b/index.ts\n@@ -1 +1 @@\n\
                     +        model, max_tokens: maxTokens, temperature, stream: false,\n";
        let hits = scan(diff);
        assert!(
            hits.is_empty(),
            "max_tokens is an ordinary LLM API parameter, not a credential: {hits:?}"
        );
    }

    #[test]
    fn scan_does_not_flag_other_benign_token_count_identifiers() {
        for line in [
            "+num_tokens: 4096,\n",
            "+token_count = 128\n",
            "+token_limit: 8192,\n",
            "+tokenizer = get_tokenizer(model)\n",
        ] {
            let diff = format!("+++ b/config.py\n@@ -1 +1 @@\n{line}");
            let hits = scan(&diff);
            assert!(hits.is_empty(), "false positive on {line:?}: {hits:?}");
        }
    }

    #[test]
    fn scan_still_flags_a_real_token_credential() {
        let diff = "+++ b/.env\n@@ -1 +1 @@\n+AUTH_TOKEN=abcdef0123456789longenough\n";
        let hits = scan(diff);
        assert_eq!(
            hits.len(),
            1,
            "a real *_TOKEN credential must still be caught: {hits:?}"
        );
    }

    #[test]
    fn contains_marker_word_rejects_a_mid_word_match() {
        assert!(!contains_marker_word("TOKENIZER", "TOKEN"));
        assert!(!contains_marker_word("MAX_TOKENS", "TOKEN"));
    }

    #[test]
    fn contains_marker_word_accepts_a_real_word_boundary_match() {
        assert!(contains_marker_word("AUTH_TOKEN", "TOKEN"));
        assert!(contains_marker_word("TOKEN", "TOKEN"));
        assert!(contains_marker_word("API_TOKEN_EXPIRY", "TOKEN"));
    }

    #[test]
    fn scan_detects_a_secret_on_a_removed_line() {
        // #155: a PR that *removes* a leaked secret still has that secret's line in the diff
        // text sent to the LLM — the scan must catch it there too, not just on added lines.
        let diff = "+++ b/config.py\n@@ -1 +1 @@\n\
                     -AWS_KEY = \"AKIAABCDEFGHIJKLMNOP\"\n\
                     +AWS_KEY = os.environ[\"AWS_KEY\"]\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "AWS access key ID"));
    }

    #[test]
    fn scan_detects_a_secret_on_a_context_line() {
        let diff = "+++ b/config.py\n@@ -1,2 +1,3 @@\n\
                     \x20AWS_KEY = \"AKIAABCDEFGHIJKLMNOP\"\n\
                     +unrelated_added_line = 1\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "AWS access key ID"));
    }

    #[test]
    fn scan_detects_a_secret_on_an_added_line_whose_own_content_starts_with_plus() {
        // #137: an added line whose content is "+AWS_KEY=..." produces the raw diff line
        // "++AWS_KEY=..." — this used to be silently skipped entirely.
        let diff = "+++ b/config.py\n@@ -1 +1 @@\n++AWS_KEY=\"AKIAABCDEFGHIJKLMNOP\"\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "AWS access key ID"));
    }

    #[test]
    fn scan_does_not_mistake_an_added_line_starting_with_plus_plus_plus_for_a_file_header() {
        // #137: mirrors input::parse_diff_stats's equivalent regression test — a hunk-body
        // added line that happens to start with "+++" must still be scanned as content, not
        // mistaken for a "+++ b/path" file header (which only appears before the first "@@").
        let diff = "diff --git a/note.txt b/note.txt\n\
                     --- a/note.txt\n\
                     +++ b/note.txt\n\
                     @@ -1,1 +1,2 @@\n\
                      line one\n\
                     +++ AWS_KEY=\"AKIAABCDEFGHIJKLMNOP\"\n";
        let hits = scan(diff);
        assert!(hits.iter().any(|h| h.pattern == "AWS access key ID"));
        assert_eq!(hits[0].file, "note.txt");
    }

    #[test]
    fn scan_text_finds_a_secret_in_plain_text_and_labels_it_with_the_given_source() {
        let hits = scan_text(
            "requirements",
            "some notes\nour key is AKIAABCDEFGHIJKLMNOP\nmore notes",
        );
        assert!(hits.iter().any(|h| h.pattern == "AWS access key ID"));
        assert_eq!(hits[0].file, "requirements");
    }

    #[test]
    fn scan_text_ignores_lines_with_no_secret_shape() {
        assert!(scan_text("conventions", "use tabs, not spaces\nprefer early returns").is_empty());
    }

    #[test]
    fn scan_returns_no_hits_on_an_ordinary_diff() {
        let diff = "+++ b/src/main.rs\n@@ -1 +1 @@\n+fn main() { println!(\"hi\"); }\n";
        assert!(scan(diff).is_empty());
    }

    #[test]
    fn redact_masks_the_middle_and_keeps_first_and_last_four_chars() {
        assert_eq!(redact("AKIAABCDEFGHIJKLMNOP"), "AKIA************MNOP");
        assert_eq!(redact("short"), "*****");
    }

    #[test]
    fn redact_does_not_panic_on_multi_byte_utf8_secrets() {
        // #138: byte-index slicing used to panic here — a Korean char is 3 bytes, so byte
        // index 4 lands mid-character.
        let value = "한글비밀번호1234567890";
        let redacted = redact(value);
        assert!(redacted.starts_with("한글비밀"));
        assert!(redacted.ends_with("7890"));
    }

    #[test]
    fn scan_finds_and_redacts_a_non_ascii_env_style_secret_without_panicking() {
        let diff = "+++ b/config.py\n@@ -1 +1 @@\n+PASSWORD=한글비밀번호1234567890\n";
        let hits = scan(diff);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].redacted.starts_with("한글비밀"));
    }
}
