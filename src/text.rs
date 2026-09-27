//! Text helpers: redaction, excerpts, hashing, timestamp parsing.

use regex::Regex;
use std::sync::LazyLock;

static SECRETS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?s)<private>.*?</private>", "[private]"),
        (r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", "[redacted:private-key]"),
        (r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", "[redacted:jwt]"),
        (r"\b(?:sk|pk|rk)-[A-Za-z0-9_-]{20,}", "[redacted:key]"),
        (r"\bgh[pousr]_[A-Za-z0-9]{30,}", "[redacted:github]"),
        (r"\bglpat-[A-Za-z0-9_-]{20,}", "[redacted:gitlab]"),
        (r"\bAKIA[0-9A-Z]{16}\b", "[redacted:aws]"),
        (r"\bxox[abprs]-[A-Za-z0-9-]{10,}", "[redacted:slack]"),
        (r"\bpk_[0-9]{6,}_[A-Z0-9]{20,}", "[redacted:clickup]"),
        (r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]{20,}=*", "Bearer [redacted]"),
        (
            r#"(?i)\b([A-Z0-9_]*(?:api[_-]?key|secret|token|passwd|password|credential)[A-Z0-9_]*)(\s*[:=]\s*["']?)[^\s"',;]{8,}"#,
            "$1$2[redacted]",
        ),
    ]
    .into_iter()
    .map(|(p, r)| (Regex::new(p).expect("valid secret regex"), r))
    .collect()
});

pub fn redact(s: &str) -> String {
    let mut out = s.to_string();
    for (re, rep) in SECRETS.iter() {
        if re.is_match(&out) {
            out = re.replace_all(&out, *rep).into_owned();
        }
    }
    out
}

/// First `max` chars, cut on a char boundary.
pub fn head(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Last `lines` lines, capped at `max` chars. Errors usually carry the signal at the end.
pub fn tail(s: &str, lines: usize, max: usize) -> String {
    let all: Vec<&str> = s.trim_end().lines().collect();
    let start = all.len().saturating_sub(lines);
    let joined = all[start..].join("\n");
    let n = joined.chars().count();
    if n <= max {
        joined
    } else {
        let skip = n - max;
        format!("…{}", joined.chars().skip(skip).collect::<String>())
    }
}

pub fn clean(s: &str, max: usize) -> String {
    redact(&head(s.trim(), max))
}

pub fn hash(s: &str) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(s.as_bytes()))
}

/// Parse RFC 3339 UTC-ish timestamps ("2026-09-27T11:13:24.101Z") into epoch millis.
pub fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut ms = 0;
    let mut i = 19;
    if b.get(19) == Some(&b'.') {
        i = 20;
        let mut scale = 100;
        while i < b.len() && b[i].is_ascii_digit() {
            ms += (b[i] - b'0') as i64 * scale;
            scale /= 10;
            i += 1;
        }
    }
    let mut offset_min = 0;
    if let Some(&c) = b.get(i)
        && (c == b'+' || c == b'-')
    {
        let oh = num(i + 1..i + 3)?;
        let om = num(i + 4..i + 6).unwrap_or(0);
        offset_min = (oh * 60 + om) * if c == b'+' { 1 } else { -1 };
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + h * 3600 + mi * 60 + se - offset_min * 60) * 1000) + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-09-27T11:13:24.101Z"), Some(1_790_507_604_101));
        assert_eq!(parse_ts("2026-09-27T12:13:24.101+01:00"), Some(1_790_507_604_101));
    }

    #[test]
    fn redacts_secrets() {
        let s = redact("export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123 and token: abcdefgh12345");
        assert!(!s.contains("abcdefghijklmnop"), "{s}");
        assert!(!s.contains("abcdefgh12345"), "{s}");
        assert_eq!(redact("keep <private>hidden</private> text"), "keep [private] text");
        assert_eq!(redact("cargo test --release"), "cargo test --release");
    }

    #[test]
    fn excerpts() {
        assert_eq!(head("héllo", 2), "hé…");
        assert_eq!(tail("a\nb\nc", 2, 100), "b\nc");
    }
}
