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

/// Why an ISO 8601 cell value has no Excel serial.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum DateError {
    /// Not a valid date, date-time, or time in a supported form.
    Invalid,
    /// A valid value with a time-zone offset other than `Z`, which would
    /// need a local time zone to convert.
    TimeZoneOffset,
    /// A valid date before the workbook's date system starts.
    BeforeEpoch,
}

// Splits a `+HH:MM` or `-HH:MM` time-zone offset from the end of a value.
fn offset(text: &str) -> Option<&str> {
    let split = text.len().checked_sub(6)?;
    let (value, offset) = (text.get(..split)?, text.get(split..)?);
    let bytes = offset.as_bytes();
    (matches!(bytes[0], b'+' | b'-')
        && bytes[3] == b':'
        && digits(offset, 1..3).is_some_and(|hours| hours <= 14)
        && digits(offset, 4..6).is_some_and(|minutes| minutes <= 59)
        && !value.is_empty())
    .then_some(value)
}

/// Converts `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS[.fraction]]` or a time-only
/// `HH:MM[:SS[.fraction]]`, each optionally ending in `Z`, to an Excel serial.
///
/// Other forms are invalid. A value that would be valid but for a time-zone
/// offset other than `Z`, or a date before the workbook's epoch, is reported
/// separately. The 1900 date system counts Excel's nonexistent 1900-02-29 as
/// serial 60, so serials from 1900-03-01 onward are one day greater than
/// elapsed days; Excel's day zero, 1899-12-31, is serial 0, and holds times
/// without a date. A time before midnight never reaches the next day's serial.
pub(super) fn excel_serial(text: &str, date1904: bool) -> Result<f64, DateError> {
    let (text, zoned) = match text.strip_suffix('Z') {
        Some(text) => (text, false),
        None => offset(text).map_or((text, false), |text| (text, true)),
    };
    let parsed = match text.split_once('T') {
        Some((day, clock)) => date(day).map(Some).zip(time(clock)),
        None if text.len() == 10 && text.as_bytes()[4] == b'-' => {
            date(text).map(|days| (Some(days), 0.0))
        }
        None => time(text).map(|fraction| (None, fraction)),
    };
    let (days, fraction) = parsed.ok_or(DateError::Invalid)?;
    if zoned {
        return Err(DateError::TimeZoneOffset);
    }
    let serial = match days {
        // A time without a date is a fraction of serial 0 in either system.
        None => 0,
        Some(days) if date1904 => match days - days_from_civil(1904, 1, 1) {
            ..0 => return Err(DateError::BeforeEpoch),
            serial => serial,
        },
        // Excel's day zero, 1899-12-31, is serial 0.
        Some(days) => match days - days_from_civil(1899, 12, 30) {
            ..=0 => return Err(DateError::BeforeEpoch),
            serial @ 1..=60 => serial - 1,
            serial => serial,
        },
    };
    // Neither rounding to the serial's precision nor a fraction of a second
    // that rounds up to 60 may carry a time before midnight into the next day.
    let next_day = (serial + 1) as f64;
    Ok((serial as f64 + fraction).min(next_day.next_down()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Month lengths come from a table independent of `date`, so a wrong
    // month length there accepts an invalid day, and a wrong day count in
    // `days_from_civil` skips or repeats a serial.
    #[test]
    fn every_calendar_day_has_the_next_serial() {
        let month_days = |year: i64, month: i64| match month {
            2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        let mut previous = 0.0;
        // The full 1900-9999 range takes seconds in a debug build.
        for year in 1900..=2100 {
            for month in 1..=12 {
                let last = month_days(year, month);
                for day in [0, last + 1] {
                    assert_eq!(
                        excel_serial(&format!("{year:04}-{month:02}-{day:02}"), false),
                        Err(DateError::Invalid),
                        "{year}-{month}-{day}"
                    );
                }
                for day in 1..=last {
                    let serial =
                        excel_serial(&format!("{year:04}-{month:02}-{day:02}"), false).unwrap();
                    // Excel counts a nonexistent 1900-02-29 as serial 60.
                    let step = if (year, month, day) == (1900, 3, 1) {
                        2.0
                    } else {
                        1.0
                    };
                    assert_eq!(serial, previous + step, "{year}-{month}-{day}");
                    previous = serial;
                }
            }
        }
        assert_eq!(previous, 73_415.0);
    }
}
