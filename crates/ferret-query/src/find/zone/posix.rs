//! POSIX TZ strings and the TZif v2/v3 footer's recurring transition rules.

use super::Offset;
use super::calendar::{civil_date, days_from_civil};

#[derive(Debug)]
pub(super) struct Posix {
    pub standard: Offset,
    daylight: Option<(Offset, Rule, Rule)>,
}

#[derive(Debug)]
struct Rule {
    day: Day,
    seconds: i32,
}

#[derive(Debug)]
enum Day {
    Julian(u16),
    Ordinal(u16),
    Month(u8, u8, u8),
}

impl Posix {
    pub fn parse(text: &[u8]) -> Option<Self> {
        let mut parser = Parser(text);
        let name = parser.name()?;
        let seconds = -parser.clock(24)?;
        let standard = Offset {
            seconds,
            name,
            daylight: false,
        };
        if parser.0.is_empty() {
            return Some(Self {
                standard,
                daylight: None,
            });
        }
        let name = parser.name()?;
        let seconds = if parser.0.first() != Some(&b',') && !parser.0.is_empty() {
            -parser.clock(24)?
        } else {
            standard.seconds + 3600
        };
        let daylight = Offset {
            seconds,
            name,
            daylight: true,
        };
        // glibc's default rules when DST is named without explicit transitions.
        let (start, end) = if parser.0.is_empty() {
            (
                Rule {
                    day: Day::Month(3, 2, 0),
                    seconds: 7200,
                },
                Rule {
                    day: Day::Month(11, 1, 0),
                    seconds: 7200,
                },
            )
        } else {
            parser.byte(b',')?;
            let start = parser.rule()?;
            parser.byte(b',')?;
            let end = parser.rule()?;
            (start, end)
        };
        parser.0.is_empty().then_some(Self {
            standard,
            daylight: Some((daylight, start, end)),
        })
    }

    pub fn at(&self, stamp: i64) -> &Offset {
        let Some((daylight, start, end)) = &self.daylight else {
            return &self.standard;
        };
        let year = civil_date(stamp.div_euclid(86400)).0;
        // Signed transition hours may put a rule outside its nominal year.
        // Find the last transition over the adjoining years, also for southern
        // DST.
        let mut last = (i64::MIN, &self.standard);
        for year in year - 1..=year + 1 {
            for (rule, before, after) in [
                (start, &self.standard, daylight),
                (end, daylight, &self.standard),
            ] {
                let transition = rule.at(year) - i64::from(before.seconds);
                if transition <= stamp && transition >= last.0 {
                    last = (transition, after);
                }
            }
        }
        last.1
    }

    pub fn offsets(&self) -> impl Iterator<Item = &Offset> {
        std::iter::once(&self.standard).chain(self.daylight.iter().map(|(offset, _, _)| offset))
    }
}

impl Rule {
    fn at(&self, year: i64) -> i64 {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let ordinal = match self.day {
            Day::Ordinal(day) => i64::from(day),
            Day::Julian(day) => i64::from(day) - 1 + i64::from(leap && day >= 60),
            Day::Month(month, week, weekday) => {
                let first = days_from_civil(year, u32::from(month), 1);
                let first_weekday = (first + 4).rem_euclid(7);
                let mut day = 1
                    + (i64::from(weekday) - first_weekday).rem_euclid(7)
                    + 7 * (i64::from(week) - 1);
                let next = if month == 12 {
                    days_from_civil(year + 1, 1, 1)
                } else {
                    days_from_civil(year, u32::from(month + 1), 1)
                };
                if first + day > next {
                    day -= 7;
                }
                first + day - 1 - days_from_civil(year, 1, 1)
            }
        };
        (days_from_civil(year, 1, 1) + ordinal) * 86400 + i64::from(self.seconds)
    }
}

struct Parser<'a>(&'a [u8]);
impl Parser<'_> {
    fn byte(&mut self, byte: u8) -> Option<()> {
        if self.0.first() != Some(&byte) {
            return None;
        }
        self.0 = &self.0[1..];
        Some(())
    }

    fn number(&mut self, max: u16) -> Option<u16> {
        let length = self.0.iter().take_while(|b| b.is_ascii_digit()).count();
        if length == 0 {
            return None;
        }
        let number = std::str::from_utf8(&self.0[..length])
            .ok()?
            .parse::<u16>()
            .ok()?;
        self.0 = &self.0[length..];
        (number <= max).then_some(number)
    }

    fn name(&mut self) -> Option<Vec<u8>> {
        let quoted = self.0.first() == Some(&b'<');
        if quoted {
            self.0 = &self.0[1..];
        }
        let length = self
            .0
            .iter()
            .take_while(|b| {
                b.is_ascii_alphabetic()
                    || (quoted && (b.is_ascii_digit() || **b == b'+' || **b == b'-'))
            })
            .count();
        if length < 3 {
            return None;
        }
        let name = self.0[..length].to_vec();
        self.0 = &self.0[length..];
        if quoted {
            self.byte(b'>')?;
        }
        Some(name)
    }

    fn clock(&mut self, max_hour: u16) -> Option<i32> {
        let sign = match self.0.first() {
            Some(b'-') => {
                self.0 = &self.0[1..];
                -1
            }
            Some(b'+') => {
                self.0 = &self.0[1..];
                1
            }
            _ => 1,
        };
        let mut seconds = i32::from(self.number(max_hour)?) * 3600;
        if self.0.first() == Some(&b':') {
            self.byte(b':')?;
            seconds += i32::from(self.number(59)?) * 60;
            if self.0.first() == Some(&b':') {
                self.byte(b':')?;
                seconds += i32::from(self.number(59)?);
            }
        }
        Some(sign * seconds)
    }

    fn rule(&mut self) -> Option<Rule> {
        let day = match self.0.first()? {
            b'M' => {
                self.byte(b'M')?;
                let month = self.number(12)? as u8;
                self.byte(b'.')?;
                let week = self.number(5)? as u8;
                self.byte(b'.')?;
                let weekday = self.number(6)? as u8;
                if month == 0 || week == 0 {
                    return None;
                }
                Day::Month(month, week, weekday)
            }
            b'J' => {
                self.byte(b'J')?;
                let day = self.number(365)?;
                if day == 0 {
                    return None;
                }
                Day::Julian(day)
            }
            _ => Day::Ordinal(self.number(365)?),
        };
        let seconds = if self.0.first() == Some(&b'/') {
            self.byte(b'/')?;
            self.clock(167)?
        } else {
            7200
        };
        Some(Rule { day, seconds })
    }
}
