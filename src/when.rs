//! The time a question about past work is about: "yesterday", "last week", "on 4 October",
//! "kemarin", "since 2026-09-28". Read from the question itself, in the asker's local time,
//! so `rvn ask` can look at what happened in that window instead of matching the word
//! "yesterday" against memory text.
//!
//! The clock and the UTC offset are parameters: replaying a question as of when it was
//! typed resolves "yesterday" against that moment, not today.

use crate::text::{civil_from_days, days_from_civil};

const DAY: i64 = 86_400_000;

/// A half-open window of epoch milliseconds, with the words that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
    /// What the question said, as resolved: "yesterday (2026-10-05)".
    pub label: String,
    /// The asker's UTC offset the window was resolved in (minutes): its days are read in it.
    pub offset_min: i64,
}

const MONTHS: [(&str, i64); 24] = [
    ("january", 1),
    ("february", 2),
    ("march", 3),
    ("april", 4),
    ("may", 5),
    ("june", 6),
    ("july", 7),
    ("august", 8),
    ("september", 9),
    ("october", 10),
    ("november", 11),
    ("december", 12),
    // Indonesian names that differ from the English ones.
    ("januari", 1),
    ("februari", 2),
    ("maret", 3),
    ("mei", 5),
    ("juni", 6),
    ("juli", 7),
    ("agustus", 8),
    ("oktober", 10),
    ("desember", 12),
    ("sept", 9),
    ("okt", 10),
    ("des", 12),
];

/// A month name or its three-letter-or-longer abbreviation ("oct", "agu"). Only read
/// next to a day number, so "may I ask" or "march on" are never dates.
fn month(word: &str) -> Option<i64> {
    let w = word.trim_end_matches('.');
    if w.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .find(|(name, _)| name.starts_with(w))
        .map(|(_, m)| *m)
}

/// The window `question` asks about, as of `now` (epoch ms) at `offset_min` minutes east
/// of UTC; None when it names no time.
pub fn window(question: &str, now: i64, offset_min: i64) -> Option<Window> {
    let q = question.to_lowercase();
    let words: Vec<&str> = q
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    let off = offset_min * 60_000;
    let today = (now + off).div_euclid(DAY);
    let at = |day: i64| day * DAY - off;
    let date = |day: i64| {
        let (y, m, d) = civil_from_days(day);
        format!("{y:04}-{m:02}-{d:02}")
    };
    let span = |from: i64, to: i64, what: &str| Window {
        start: at(from),
        end: at(to).min(now),
        label: if to - from == 1 {
            format!("{what} ({})", date(from))
        } else {
            format!("{what} ({} to {})", date(from), date(to - 1))
        },
        offset_min,
    };
    let has = |w: &str| words.contains(&w);
    let pair = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);

    // ISO dates: "since 2026-09-28", "on 2026-10-04".
    let iso: Vec<i64> = words
        .iter()
        .filter_map(|w| {
            let p: Vec<&str> = w.split('-').collect();
            match p.as_slice() {
                [y, m, d] if y.len() == 4 => Some(days_from_civil(
                    y.parse().ok()?,
                    m.parse().ok()?,
                    d.parse().ok()?,
                )),
                _ => None,
            }
        })
        .collect();
    // "4 October", "October 4", "4 okt": a day number next to a month name. Without a
    // year, the most recent such date not after today.
    let named = words.windows(2).find_map(|p| {
        let (d, m) = match (p[0].parse::<i64>(), p[1].parse::<i64>()) {
            (Ok(d), Err(_)) => (d, month(p[1])?),
            (Err(_), Ok(d)) => (d, month(p[0])?),
            _ => return None,
        };
        if !(1..=31).contains(&d) {
            return None;
        }
        let (y, _, _) = civil_from_days(today);
        let this = days_from_civil(y, m, d);
        Some(if this > today {
            days_from_civil(y - 1, m, d)
        } else {
            this
        })
    });
    let since = has("since") || has("sejak");
    if let Some(&d) = iso.first().or(named.as_ref()) {
        return Some(if since {
            span(d, today + 1, "since")
        } else {
            span(d, d + 1, "on")
        });
    }

    if has("today") || pair("hari", "ini") || has("tadi") {
        return Some(span(today, today + 1, "today"));
    }
    if has("yesterday") || has("kemarin") || has("kemaren") {
        // "yesterday morning until now" / "dari kemarin": through to now.
        let through = has("now") || has("sekarang") || has("until") || has("sampai");
        return Some(if through {
            span(today - 1, today + 1, "since yesterday")
        } else {
            span(today - 1, today, "yesterday")
        });
    }
    // Weeks start on Monday (1970-01-01 was a Thursday).
    let monday = today - (today + 3).rem_euclid(7);
    if pair("last", "week") || pair("minggu", "lalu") || pair("pekan", "lalu") {
        return Some(span(monday - 7, monday, "last week"));
    }
    if pair("this", "week") || pair("minggu", "ini") || pair("pekan", "ini") {
        return Some(span(monday, today + 1, "this week"));
    }
    if pair("last", "month") || pair("bulan", "lalu") {
        let (y, m, _) = civil_from_days(today);
        let first = days_from_civil(y, m, 1);
        let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
        return Some(span(days_from_civil(py, pm, 1), first, "last month"));
    }
    // "last 3 days", "past 2 weeks", "3 hari terakhir".
    for w in words.windows(3) {
        let n = match (w[0], w[1].parse::<i64>(), w[2]) {
            ("last" | "past", Ok(n), unit) => Some((n, unit)),
            _ => None,
        }
        .or_else(|| match (w[0].parse::<i64>(), w[1], w[2]) {
            (Ok(n), unit, "terakhir") => Some((n, unit)),
            _ => None,
        });
        if let Some((n, unit)) = n.filter(|(n, _)| (1..=366).contains(n)) {
            let days = match unit.trim_end_matches('s') {
                "day" | "hari" => n,
                "week" | "minggu" | "pekan" => n * 7,
                _ => continue,
            };
            return Some(span(
                today + 1 - days,
                today + 1,
                &format!("last {n} {unit}"),
            ));
        }
    }
    None
}

/// Minutes east of UTC on this machine now, for resolving the asker's "yesterday".
#[cfg(unix)]
pub fn local_offset_min() -> i64 {
    let t = (crate::db::now_ms() / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64 / 60
}

#[cfg(not(unix))]
pub fn local_offset_min() -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIB: i64 = 7 * 60;
    /// 2026-10-06 09:00 WIB (Tuesday) = 02:00 UTC.
    fn tue() -> i64 {
        crate::text::parse_ts("2026-10-06T02:00:00Z").unwrap()
    }
    fn ms(s: &str) -> i64 {
        crate::text::parse_ts(s).unwrap()
    }

    #[test]
    fn yesterday_is_the_whole_previous_local_day() {
        let w = window("what did we do on ravnori yesterday?", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2026-10-05T00:00:00+07:00"));
        assert_eq!(w.end, ms("2026-10-06T00:00:00+07:00"));
        assert_eq!(w.label, "yesterday (2026-10-05)");
        // Indonesian, and the same moment seen from UTC is still the asker's day.
        assert_eq!(window("kemarin kita ngapain?", tue(), WIB), Some(w));
    }

    #[test]
    fn yesterday_until_now_runs_to_the_moment_asked() {
        let w = window(
            "recap my daily update from yesterday morning until now",
            tue(),
            WIB,
        )
        .unwrap();
        assert_eq!(w.start, ms("2026-10-05T00:00:00+07:00"));
        assert_eq!(w.end, tue());
    }

    #[test]
    fn a_named_date_without_a_year_is_the_latest_one_not_in_the_future() {
        let w = window("what did we ship in ravnori on 4 October?", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2026-10-04T00:00:00+07:00"));
        assert_eq!(w.end, ms("2026-10-05T00:00:00+07:00"));
        let w = window("what happened on December 24", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2025-12-24T00:00:00+07:00"));
        let w = window("apa yang kita kerjakan tanggal 15 agustus", tue(), WIB).unwrap();
        assert_eq!(w.label, "on (2026-08-15)");
    }

    #[test]
    fn iso_dates_and_since() {
        let w = window("what changed since 2026-09-28", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2026-09-28T00:00:00+07:00"));
        assert_eq!(w.end, tue());
        let w = window("on 2026-09-30 what broke", tue(), WIB).unwrap();
        assert_eq!(w.end - w.start, DAY);
    }

    #[test]
    fn weeks_start_on_monday() {
        let w = window("what did we work on in ravnori last week?", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2026-09-28T00:00:00+07:00"));
        assert_eq!(w.end, ms("2026-10-05T00:00:00+07:00"));
        let w = window("minggu ini", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2026-10-05T00:00:00+07:00"));
    }

    #[test]
    fn last_n_days() {
        let w = window("what did we fix in the last 3 days", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2026-10-04T00:00:00+07:00"));
        let w = window("2 minggu terakhir", tue(), WIB).unwrap();
        assert_eq!(w.start, ms("2026-09-23T00:00:00+07:00"));
    }

    #[test]
    fn questions_without_a_time_have_no_window() {
        for q in [
            "why was reranking not shipped for prompt recall?",
            "why is the rename to galur on hold?",
            "may I ask why we chose AGPL",
            "how many days does the trial last",
            "march through the backlog",
        ] {
            assert_eq!(window(q, tue(), WIB), None, "{q}");
        }
    }
}
