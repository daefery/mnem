//! Text helpers: redaction, excerpts, hashing, timestamp parsing.

use regex::Regex;
use std::sync::LazyLock;

static SECRETS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?s)<private>.*?</private>", "[private]"),
        // An unterminated block hides everything after it.
        (r"(?s)<private>.*", "[private]"),
        (r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", "[redacted:private-key]"),
        (r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*", "[redacted:private-key]"),
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

/// Redaction must see whole tokens. Redact a generous window cut on whitespace (so no
/// secret straddles the window edge), then truncate the redacted text.
const WINDOW: usize = 64 * 1024;

fn window_head(s: &str) -> &str {
    if s.len() <= WINDOW {
        return s;
    }
    let mut end = WINDOW;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let cut = s[..end].rfind(char::is_whitespace).unwrap_or(0);
    &s[..cut]
}

fn window_tail(s: &str) -> &str {
    if s.len() <= WINDOW {
        return s;
    }
    let mut start = s.len() - WINDOW;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    let cut = s[start..]
        .find(char::is_whitespace)
        .map(|i| start + i)
        .unwrap_or(s.len());
    &s[cut..]
}

/// Redacted excerpt from the start of `s`.
pub fn clean(s: &str, max: usize) -> String {
    head(&redact(window_head(s.trim())), max)
}

/// Redacted excerpt from the end of `s`; errors usually carry the signal at the end.
pub fn clean_tail(s: &str, lines: usize, max: usize) -> String {
    tail(&redact(window_tail(s)), lines, max)
}

/// Redacted error excerpt: the first `head_n` lines (what failed) and the last `tail_n`
/// lines (the verdict), each line capped, total capped at `max`.
pub fn clean_error(s: &str, head_n: usize, tail_n: usize, max: usize) -> String {
    let s = s.trim();
    let red = if s.len() <= 2 * WINDOW {
        redact(s)
    } else {
        format!("{}\n…\n{}", redact(window_head(s)), redact(window_tail(s)))
    };
    let lines: Vec<&str> = red.lines().filter(|l| !l.trim().is_empty()).collect();
    let pick: Vec<String> = if lines.len() <= head_n + tail_n {
        lines.iter().map(|l| head(l, 200)).collect()
    } else {
        let mut v: Vec<String> = lines[..head_n].iter().map(|l| head(l, 200)).collect();
        v.push("…".into());
        v.extend(lines[lines.len() - tail_n..].iter().map(|l| head(l, 200)));
        v
    };
    head(&pick.join("\n"), max)
}

/// Replace pasted terminal dumps with a size marker; they drown out the actual ask.
pub fn strip_pasted(s: &str) -> String {
    static PASTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<pasted_content[^>]*>(.*?)</pasted_content>").expect("valid regex")
    });
    PASTED
        .replace_all(s, |c: &regex::Captures| {
            format!("[pasted {} chars]", c[1].len())
        })
        .into_owned()
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
        assert_eq!(
            parse_ts("2026-09-27T11:13:24.101Z"),
            Some(1_790_507_604_101)
        );
        assert_eq!(
            parse_ts("2026-09-27T12:13:24.101+01:00"),
            Some(1_790_507_604_101)
        );
    }

    #[test]
    fn redacts_secrets() {
        let s = redact(
            "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz123 and token: abcdefgh12345",
        );
        assert!(!s.contains("abcdefghijklmnop"), "{s}");
        assert!(!s.contains("abcdefgh12345"), "{s}");
        assert_eq!(
            redact("keep <private>hidden</private> text"),
            "keep [private] text"
        );
        assert_eq!(redact("cargo test --release"), "cargo test --release");
    }

    #[test]
    fn redacts_before_truncating() {
        let key = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        let s = format!("{}{key}", "x ".repeat(10));
        let out = clean(&s, 25);
        assert!(!out.contains("sk-abc"), "{out}");
        let out = clean("<private>my secret plan that is long", 20);
        assert!(!out.contains("secret"), "{out}");
        let big = format!("{} {key} tail", "word ".repeat(20_000));
        assert!(!clean_tail(&big, 3, 100).contains("sk-abc"));
        assert!(!clean(&big, 200_000).contains("sk-abc"));
    }

    #[test]
    fn error_excerpt_keeps_head_and_tail() {
        let s = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            clean_error(&s, 2, 2, 800),
            "line 1\nline 2\n…\nline 19\nline 20"
        );
    }

    #[test]
    fn strips_pasted() {
        assert_eq!(
            strip_pasted("see <pasted_content id=1>abc</pasted_content> now"),
            "see [pasted 3 chars] now"
        );
    }

    #[test]
    fn excerpts() {
        assert_eq!(head("héllo", 2), "hé…");
        assert_eq!(tail("a\nb\nc", 2, 100), "b\nc");
    }
}
