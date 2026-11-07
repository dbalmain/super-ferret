//! `-newerXt`'s date argument: the bounded GNU date subset the find corpus
//! uses, relative or absolute, resolved against the run's start time.

use super::*;
use crate::find::zone::{
    calendar::{civil_date, days_from_civil},
    local,
};

/// Parses the deliberately bounded GNU date subset used by the find corpus.
pub(super) fn parse_date(value: &OsStr, now: SystemTime, guess: &mut i32) -> Option<i128> {
    let text = std::str::from_utf8(value.as_bytes()).ok()?.trim();
    let now_ns = now.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i128;
    let timestamp_ns = if text == "now" {
        return Some(now_ns);
    } else if text == "today" || text == "yesterday" {
        if text == "today" {
            now_ns
        } else {
            relative_stamp("1 day", now_ns, guess)?
        }
    } else if let Some(epoch) = text.strip_prefix('@') {
        epoch.parse::<i64>().ok()? as i128 * 1_000_000_000
    } else if let Some(relative) = text.strip_prefix("now-") {
        relative_stamp(relative, now_ns, guess)?
    } else if let Some(relative) = text.strip_prefix('-') {
        relative_stamp(relative, now_ns, guess)?
    } else if let Some(relative) = text.strip_suffix(" ago") {
        if let Some(stamp) = relative_stamp(relative, now_ns, guess) {
            stamp
        } else {
            calendar_months_ago(relative, now, guess)?
        }
    } else {
        parse_local_absolute(text, now, guess)? as i128 * 1_000_000_000
    };
    Some(timestamp_ns)
}

fn relative_stamp(value: &str, now_ns: i128, guess: &mut i32) -> Option<i128> {
    let delta = relative_seconds(value)?;
    if value.split_whitespace().nth(1)?.trim_end_matches('s') != "day" {
        return Some(now_ns - i128::from(delta) * 1_000_000_000);
    }
    let seconds = i64::try_from(now_ns.div_euclid(1_000_000_000)).ok()?;
    let zone = local();
    *guess = zone.offset(seconds);
    let wall = zone.wall(seconds).checked_sub(delta)?;
    Some(i128::from(zone.resolve(wall, guess)?) * 1_000_000_000 + now_ns.rem_euclid(1_000_000_000))
}

fn relative_seconds(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let amount: i64 = parts.next()?.parse().ok()?;
    let unit = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let unit = unit.trim_end_matches('s');
    let multiplier = match unit {
        "second" | "sec" => 1,
        "minute" | "min" => 60,
        "hour" | "hr" => 3600,
        "day" => 86400,
        _ => return None,
    };
    amount.checked_mul(multiplier)
}

fn parse_local_absolute(value: &str, now: SystemTime, guess: &mut i32) -> Option<i64> {
    // Explicit ISO offsets and Z bypass the process zone.
    if let Some((date, clock)) = value.split_once('T') {
        if clock.ends_with('Z') {
            return parse_absolute(&format!("{date} {}", clock.strip_suffix('Z')?), now);
        }
        if clock
            .char_indices()
            .skip(1)
            .any(|(_, c)| c == '+' || c == '-')
        {
            return parse_absolute(value, now);
        }
    }
    local().resolve(parse_absolute(value, now)?, guess)
}

fn parse_absolute(value: &str, now: SystemTime) -> Option<i64> {
    if let Some((date, clock)) = value.split_once('T') {
        let clock = clock.strip_suffix('Z').unwrap_or(clock);
        let zone_at = clock
            .char_indices()
            .skip(1)
            .filter(|(_, c)| *c == '+' || *c == '-')
            .map(|(i, _)| i)
            .last();
        let (clock, offset) = if let Some(i) = zone_at {
            (&clock[..i], Some(&clock[i..]))
        } else {
            (clock, None)
        };
        let base = parse_absolute(&format!("{date} {clock}"), now)?;
        let zone = if let Some(offset) = offset {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let mut parts = offset[1..].split(':');
            let hours: i64 = parts.next()?.parse().ok()?;
            let minutes: i64 = parts.next()?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        } else {
            0
        };
        return Some(base - zone);
    }
    if let Some((month, rest)) = value.split_once(' ')
        && month.len() == 3
        && month.as_bytes()[0].is_ascii_alphabetic()
    {
        let month_num = match month.to_ascii_lowercase().as_str() {
            "jan" => 1,
            "feb" => 2,
            "mar" => 3,
            "apr" => 4,
            "may" => 5,
            "jun" => 6,
            "jul" => 7,
            "aug" => 8,
            "sep" => 9,
            "oct" => 10,
            "nov" => 11,
            "dec" => 12,
            _ => return None,
        };
        let rest = rest.trim_start();
        let (day_text, tail) = rest
            .split_once(',')
            .map_or_else(|| rest.split_once(' '), |(d, t)| Some((d, t)))?;
        let day: u32 = day_text.trim().parse().ok()?;
        let (year, time) = if rest.contains(',') {
            let mut parts = tail.split_whitespace();
            let year = parts.next()?.parse().ok()?;
            (year, parts.next())
        } else {
            let year = civil_year(now)?;
            (year, Some(tail.trim()))
        };
        let date_seconds = days_from_civil(year, month_num, day).checked_mul(86_400)?;
        if let Some(time) = time {
            let mut t = time.split(':');
            let h: i64 = t.next()?.parse().ok()?;
            let m: i64 = t.next()?.parse().ok()?;
            let s: i64 = t.next().map(str::parse).transpose().ok()?.unwrap_or(0);
            return Some(date_seconds + h * 3600 + m * 60 + s);
        }
        return Some(date_seconds);
    }
    let (date, time) = value
        .split_once(' ')
        .map_or((value, None), |(d, t)| (d, Some(t)));
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let mut seconds = days.checked_mul(86_400)?;
    if let Some(time) = time {
        let mut t = time.split(':');
        let hour: i64 = t.next()?.parse().ok()?;
        let minute: i64 = t.next()?.parse().ok()?;
        let second: i64 = t.next().map(str::parse).transpose().ok()?.unwrap_or(0);
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        seconds += hour * 3600 + minute * 60 + second;
        if t.next().is_some() {
            return None;
        }
    }
    Some(seconds)
}

fn civil_year(now: SystemTime) -> Option<i64> {
    let days = local()
        .wall(now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
        .div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    Some(year)
}

fn calendar_months_ago(value: &str, now: SystemTime, guess: &mut i32) -> Option<i128> {
    let mut parts = value.split_whitespace();
    let count: i64 = parts.next()?.parse().ok()?;
    if parts.next()?.trim_end_matches('s') != "month" || parts.next().is_some() {
        return None;
    }
    let seconds = local().wall(now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64);
    let days = seconds.div_euclid(86_400);
    let seconds_in_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date(days);
    let month_index = year
        .checked_mul(12)?
        .checked_add(month as i64 - 1)?
        .checked_sub(count)?;
    let new_year = month_index.div_euclid(12);
    let new_month = month_index.rem_euclid(12) as u32 + 1;
    let max_day = match new_month {
        2 if new_year % 4 == 0 && (new_year % 100 != 0 || new_year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    let new_day = day.min(max_day);
    Some(
        local().resolve(
            days_from_civil(new_year, new_month, new_day) * 86_400 + seconds_in_day,
            guess,
        )? as i128
            * 1_000_000_000,
    )
}
