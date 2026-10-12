//! ISO 8601 cell values (`t="d"`) as Excel serial date numbers.

// Days since 1970-01-01 in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// A fixed-width run of ASCII digits; `get` keeps slicing on character bounds.
fn digits(text: &str, range: std::ops::Range<usize>) -> Option<i64> {
    let text = text.get(range)?;
    text.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| text.parse().ok())?
}

// `YYYY-MM-DD`, validated against the Gregorian calendar.
fn date(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = digits(text, 0..4)?;
    let month = digits(text, 5..7)?;
    let day = digits(text, 8..10)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    (1..=month_days)
        .contains(&day)
        .then(|| days_from_civil(year, month, day))
}

// `HH:MM[:SS[.fraction]]` as a fraction of a day.
fn time(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    if bytes.len() < 5 || bytes[2] != b':' {
        return None;
    }
    let hours = digits(text, 0..2)?;
    let minutes = digits(text, 3..5)?;
    let seconds = match text.get(5..)? {
        "" => 0.0,
        rest => {
            // `:SS`, optionally followed by `.` and at least one digit.
            let rest = rest.strip_prefix(':')?;
            let whole = digits(rest, 0..2)?;
            let fraction = match rest.get(2..)? {
                "" => "0",
                fraction => fraction.strip_prefix('.')?,
            };
            if whole > 59
                || fraction.is_empty()
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            rest.parse::<f64>().ok()?
        }
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((((hours * 60 + minutes) * 60) as f64 + seconds) / 86_400.0)
}

/// Converts `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS[.fraction]]` or a time-only
/// `HH:MM[:SS[.fraction]]`, each optionally ending in `Z`, to an Excel serial.
///
/// Other UTC offsets and forms, and dates before the workbook's epoch, return
/// `None`. The 1900 date system counts Excel's nonexistent 1900-02-29 as serial
/// 60, so serials from 1900-03-01 onward are one day greater than elapsed days.
pub(super) fn excel_serial(text: &str, date1904: bool) -> Option<f64> {
    let text = text.strip_suffix('Z').unwrap_or(text);
    let (days, fraction) = match text.split_once('T') {
        Some((day, clock)) => (Some(date(day)?), time(clock)?),
        None if text.len() == 10 && text.as_bytes()[4] == b'-' => (Some(date(text)?), 0.0),
        None => (None, time(text)?),
    };
    let serial = match days {
        None => 0,
        Some(days) if date1904 => {
            let serial = days - days_from_civil(1904, 1, 1);
            if serial < 0 {
                return None;
            }
            serial
        }
        Some(days) => match days - days_from_civil(1899, 12, 30) {
            ..=1 => return None,
            serial @ 2..=60 => serial - 1,
            serial => serial,
        },
    };
    Some(serial as f64 + fraction)
}
