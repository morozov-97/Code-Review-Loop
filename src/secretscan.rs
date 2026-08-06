//! #122: the diff (and any --requirements/--conventions content) is sent verbatim to an
//! external LLM provider with no redaction. This is a best-effort, local, pattern-based scan
//! over the diff's *added* lines only (removed/context lines aren't what's about to be sent
//! forward as new content) that runs before any LLM call, so an accidentally-committed secret
//! doesn't leave the machine silently. It is not a substitute for a real secret scanner
//! (gitleaks/trufflehog) in CI — see the README caveat this links to.

pub(crate) struct SecretHit {
    pub(crate) file: String,
    pub(crate) pattern: &'static str,
    /// First/last 4 chars kept, the rest masked — enough to eyeball which secret it is without
    /// putting the full value in a terminal/log/report.
    pub(crate) redacted: String,
}

pub(crate) fn scan(diff: &str) -> Vec<SecretHit> {
    let mut hits = Vec::new();
    let mut current_file = String::from("(unknown file)");
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            if path != "/dev/null" {
                current_file = path.to_string();
            }
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if added.starts_with('+') {
            continue; // "+++ " header lines already handled above; other "++..." isn't added content worth scanning twice
        }
        for (pattern, value) in find_secrets(added) {
            hits.push(SecretHit {
                file: current_file.clone(),
                pattern,
                redacted: redact(value),
            });
        }
    }
    hits
}

fn redact(value: &str) -> String {
    if value.len() <= 8 {
        return "*".repeat(value.len());
    }
    format!(
        "{}{}{}",
        &value[..4],
        "*".repeat(value.len() - 8),
        &value[value.len() - 4..]
    )
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
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        found.push(("PEM private key block", line.trim()));
    }
    if let Some(m) = find_jwt(line) {
        found.push(("JWT-shaped token", m));
    }
    if let Some(m) = find_env_style_secret(line) {
        found.push(("assigned secret-like value", m));
    }

    found
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

/// Catches `.env`-style `KEY=value` (or `KEY: "value"` in YAML/JSON-ish config) lines where the
/// key name looks secret-flavored and the value isn't an obvious placeholder or an
/// interpolation reference like `${VAR}`/`$VAR`.
fn find_env_style_secret(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let sep_idx = trimmed
        .find(['=', ':'])
        .filter(|&i| i > 0 && i < trimmed.len() - 1)?;
    let key = trimmed[..sep_idx].trim();
    let key_upper = key.to_ascii_uppercase();
    if !SECRET_KEY_MARKERS.iter().any(|m| key_upper.contains(m)) {
        return None;
    }
    let value = trimmed[sep_idx + 1..]
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == ',');
    if value.len() < 8 {
        return None;
    }
    if value.starts_with('$') || value.starts_with('{') || value.starts_with('<') {
        return None; // interpolated/templated, not a literal secret
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

    #[test]
    fn scan_ignores_removed_and_context_lines() {
        let diff = "+++ b/config.py\n@@ -1 +1 @@\n\
                     -AWS_KEY = \"AKIAABCDEFGHIJKLMNOP\"\n\
                     +AWS_KEY = os.environ[\"AWS_KEY\"]\n";
        let hits = scan(diff);
        assert!(hits.is_empty());
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
}
