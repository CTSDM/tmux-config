//! The local UTC offset, for the event log's times, read from the zone file
//! (TZif, RFC 8536) as libc would: `$TZ` naming a zone, else
//! `/etc/localtime`. In process: the daemon starts no program for it.
//! Transitions come from the file; after the last one, from the rule in its
//! footer (`CET-1CEST,M3.5.0,M10.5.0/3`). Anything unreadable is UTC.

use std::fs;
use std::path::PathBuf;

pub struct Zone {
    /// Transition times (epoch seconds) and the offset from each on.
    transitions: Vec<(i64, i64)>,
    /// The offset before the first transition.
    initial: i64,
    footer: Option<Rule>,
}

/// A POSIX TZ rule: standard offset and, if any, daylight saving time.
#[derive(Debug, Clone, PartialEq)]
struct Rule {
    std: i64,
    dst: Option<(i64, Change, Change)>,
}

/// `Mm.w.d/time`: day `d` (0 = Sunday) of week `w` (5 = last) of month `m`,
/// at `time` seconds of local time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Change {
    month: i64,
    week: i64,
    day: i64,
    time: i64,
}

impl Zone {
    /// The machine's zone; `None` when no zone file can be read (UTC).
    pub fn local() -> Option<Zone> {
        let path = match std::env::var("TZ") {
            // Set but empty: UTC, as glibc.
            Ok(tz) if tz.is_empty() => return None,
            Ok(tz) => {
                let name = tz.strip_prefix(':').unwrap_or(&tz);
                if name.starts_with('/') {
                    PathBuf::from(name)
                } else if !name.contains("..") {
                    PathBuf::from("/usr/share/zoneinfo").join(name)
                } else {
                    return None;
                }
            }
            _ => PathBuf::from("/etc/localtime"),
        };
        match fs::read(&path) {
            Ok(data) => Zone::parse(&data),
            // TZ may be a rule itself (`CET-1CEST,...`).
            Err(_) => {
                let tz = std::env::var("TZ").ok()?;
                Some(Zone {
                    transitions: Vec::new(),
                    initial: 0,
                    footer: Some(Rule::parse(&tz)?),
                })
            }
        }
    }

    /// Seconds east of UTC at `t` (epoch seconds).
    pub fn offset(&self, t: i64) -> i64 {
        let i = self.transitions.partition_point(|(at, _)| *at <= t);
        match (i, &self.footer) {
            (i, Some(rule)) if i == self.transitions.len() => rule.offset(t),
            (0, _) => self.initial,
            (i, _) => self.transitions[i - 1].1,
        }
    }

    fn parse(data: &[u8]) -> Option<Zone> {
        let header = |at: usize| -> Option<(u8, [usize; 6])> {
            if data.get(at..at + 4)? != b"TZif" {
                return None;
            }
            let mut counts = [0; 6];
            for (i, c) in counts.iter_mut().enumerate() {
                let b = data.get(at + 20 + 4 * i..at + 24 + 4 * i)?;
                *c = u32::from_be_bytes(b.try_into().ok()?) as usize;
            }
            Some((*data.get(at + 4)?, counts))
        };
        let (version, counts) = header(0)?;
        // Version 2 and later repeat the data with 64-bit times.
        let (at, width) = if version >= b'2' {
            let [isut, isstd, leap, time, typ, chars] = counts;
            (44 + time * 5 + typ * 6 + chars + leap * 8 + isstd + isut, 8)
        } else {
            (0, 4)
        };
        let (_, [isut, isstd, leap, time, typ, chars]) = header(at)?;
        let body = at + 44;
        let int = |at: usize, width: usize| -> Option<i64> {
            let b = data.get(at..at + width)?;
            Some(match width {
                8 => i64::from_be_bytes(b.try_into().ok()?),
                _ => i64::from(i32::from_be_bytes(b.try_into().ok()?)),
            })
        };
        let types_at = body + time * width + time;
        let utoff = |i: usize| int(types_at + 6 * i, 4);
        let mut transitions = Vec::with_capacity(time);
        for i in 0..time {
            let t = int(body + i * width, width)?;
            let typ = *data.get(body + time * width + i)? as usize;
            transitions.push((t, utoff(typ)?));
        }
        let end = types_at + typ * 6 + chars + leap * (width + 4) + isstd + isut;
        let footer = (version >= b'2')
            .then(|| data.get(end..))
            .flatten()
            .and_then(|f| std::str::from_utf8(f).ok())
            .and_then(|f| Rule::parse(f.trim_matches('\n')));
        Some(Zone {
            transitions,
            initial: if typ > 0 { utoff(0)? } else { 0 },
            footer,
        })
    }
}

impl Rule {
    /// `CET-1CEST,M3.5.0,M10.5.0/3`, `<+0530>-5:30`, `EST5EDT,M3.2.0,M11.1.0`.
    /// Julian-day rules (`Jn`, `n`) are not used by any zone today: `None`.
    fn parse(tz: &str) -> Option<Rule> {
        let mut s = tz;
        name(&mut s)?;
        let std = -time(&mut s)?;
        if s.is_empty() {
            return Some(Rule { std, dst: None });
        }
        name(&mut s)?;
        let dst = if s.starts_with(',') {
            std + 3600
        } else {
            -time(&mut s)?
        };
        let change = |s: &mut &str| -> Option<Change> {
            let rest = s.strip_prefix(",M")?;
            let (rule, tail) = rest.split_at(rest.find([',', '/']).unwrap_or(rest.len()));
            let mut parts = rule.split('.').map(|p| p.parse::<i64>().ok());
            let (month, week, day) = (parts.next()??, parts.next()??, parts.next()??);
            *s = tail;
            let time = match s.strip_prefix('/') {
                Some(t) => {
                    *s = t;
                    time(s)?
                }
                None => 7200,
            };
            Some(Change {
                month,
                week,
                day,
                time,
            })
        };
        let start = change(&mut s)?;
        let end = change(&mut s)?;
        s.is_empty().then_some(Rule {
            std,
            dst: Some((dst, start, end)),
        })
    }

    fn offset(&self, t: i64) -> i64 {
        let Some((dst, start, end)) = self.dst else {
            return self.std;
        };
        let (year, _, _) = civil_from_days((t + self.std).div_euclid(86400));
        // Start is given in standard time, end in daylight saving time.
        let on = start.at(year) - self.std;
        let off = end.at(year) - dst;
        let in_dst = if on < off {
            on <= t && t < off
        } else {
            !(off <= t && t < on)
        };
        if in_dst { dst } else { self.std }
    }
}

impl Change {
    /// Local seconds since the epoch at which it happens in `year`.
    fn at(self, year: i64) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        let weekday = (first + 4).rem_euclid(7); // 1970-01-01 was a Thursday
        let mut day = first + (self.day - weekday).rem_euclid(7) + 7 * (self.week - 1);
        let next_month = if self.month == 12 {
            days_from_civil(year + 1, 1, 1)
        } else {
            days_from_civil(year, self.month + 1, 1)
        };
        while day >= next_month {
            day -= 7; // week 5: the last one
        }
        day * 86400 + self.time
    }
}

/// A zone abbreviation: letters, or anything between `<` and `>`.
fn name(s: &mut &str) -> Option<()> {
    let len = if let Some(rest) = s.strip_prefix('<') {
        rest.find('>')? + 2
    } else {
        s.find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(s.len())
    };
    (len >= 3).then(|| *s = &s[len..])
}

/// `[+-]hh[:mm[:ss]]` in seconds.
fn time(s: &mut &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'-' => (-1, &s[1..]),
        b'+' => (1, &s[1..]),
        _ => (1, *s),
    };
    let len = rest
        .find(|c: char| !c.is_ascii_digit() && c != ':')
        .unwrap_or(rest.len());
    let mut total = 0;
    for (i, part) in rest[..len].split(':').enumerate() {
        total += part.parse::<i64>().ok()? * [3600, 60, 1].get(i)?;
    }
    *s = &rest[len..];
    Some(sign * total)
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian
/// (Howard Hinnant's `civil_from_days`).
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// (year, month, day) → days since 1970-01-01 (`days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn at(days: i64, hour: i64) -> i64 {
        days * 86400 + hour * 3600
    }

    #[test]
    fn days_both_ways() {
        for days in [-1, 0, 19_782, 20_721, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(2026, 3, 29), 20_541);
    }

    #[test]
    fn posix_rules() {
        let madrid = Rule::parse("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        // 2026: from 29 March 01:00 UTC to 25 October 01:00 UTC.
        let mar29 = days_from_civil(2026, 3, 29);
        let oct25 = days_from_civil(2026, 10, 25);
        assert_eq!(madrid.offset(at(mar29, 1) - 1), 3600);
        assert_eq!(madrid.offset(at(mar29, 1)), 7200);
        assert_eq!(madrid.offset(at(oct25, 1) - 1), 7200);
        assert_eq!(madrid.offset(at(oct25, 1)), 3600);
        let new_york = Rule::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(
            new_york.offset(at(days_from_civil(2026, 7, 1), 12)),
            -4 * 3600
        );
        assert_eq!(
            new_york.offset(at(days_from_civil(2026, 1, 1), 12)),
            -5 * 3600
        );
        // Southern hemisphere: summer time across the new year.
        let sydney = Rule::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.offset(at(days_from_civil(2026, 1, 1), 0)), 11 * 3600);
        assert_eq!(sydney.offset(at(days_from_civil(2026, 7, 1), 0)), 10 * 3600);
        let india = Rule::parse("<+0530>-5:30").unwrap();
        assert_eq!(india.offset(0), 19_800);
        assert_eq!(Rule::parse("UTC0").unwrap().offset(0), 0);
        assert!(Rule::parse("nonsense").is_none());
    }

    /// The machine's own zone file, against what libc says.
    #[test]
    fn local_zone_like_date() {
        let Some(zone) = Zone::local() else { return };
        for t in [1_767_225_600, 1_782_864_000, 4_102_444_800] {
            let out = std::process::Command::new("date")
                .args(["-d", &format!("@{t}"), "+%z"])
                .output()
                .unwrap();
            let z = String::from_utf8_lossy(&out.stdout);
            let z = z.trim();
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let expected = sign
                * (z[1..3].parse::<i64>().unwrap() * 3600 + z[3..5].parse::<i64>().unwrap() * 60);
            assert_eq!(zone.offset(t), expected, "at {t}");
        }
    }
}
