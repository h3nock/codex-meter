use std::time::SystemTime;

use chrono::{DateTime, Local};

pub fn is_date_label(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
        && value.get(0..4).is_some_and(is_ascii_digits)
        && value.get(5..7).is_some_and(is_ascii_digits)
        && value.get(8..10).is_some_and(is_ascii_digits)
        && date_days(value).is_some_and(|days| date_string_from_days(days) == value)
}

pub fn local_date_from_timestamp(timestamp: &str) -> Option<String> {
    let datetime = DateTime::parse_from_rfc3339(timestamp).ok()?;
    Some(format_date(datetime.with_timezone(&Local).date_naive()))
}

pub fn local_date_from_system_time(time: SystemTime) -> Option<String> {
    let datetime = DateTime::<Local>::from(time);
    Some(format_date(datetime.date_naive()))
}

pub fn local_date_from_unix_seconds(seconds: u64) -> Option<String> {
    let seconds = i64::try_from(seconds).ok()?;
    let datetime = DateTime::from_timestamp(seconds, 0)?;
    Some(format_date(datetime.with_timezone(&Local).date_naive()))
}

pub fn local_day_from_system_time(time: SystemTime) -> Option<i64> {
    local_date_from_system_time(time).and_then(|date| date_days(&date))
}

pub fn local_today_days() -> i64 {
    local_day_from_system_time(SystemTime::now()).unwrap_or(0)
}

pub fn date_days(date: &str) -> Option<i64> {
    let year = date.get(0..4)?.parse::<i32>().ok()?;
    let month = date.get(5..7)?.parse::<u32>().ok()?;
    let day = date.get(8..10)?.parse::<u32>().ok()?;
    Some(date_days_parts(year, month, day))
}

pub fn date_string_from_days(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn is_ascii_digits(value: &str) -> bool {
    value.as_bytes().iter().all(u8::is_ascii_digit)
}

fn format_date(date: chrono::NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn date_days_parts(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_dates() {
        let days = date_days("2026-06-03").expect("valid date");
        assert_eq!(date_string_from_days(days), "2026-06-03");
    }

    #[test]
    fn validates_date_labels() {
        assert!(is_date_label("2026-06-03"));
        assert!(!is_date_label("2026-99-03"));
        assert!(!is_date_label("2026-06-3"));
    }

    #[test]
    fn converts_timestamp_to_machine_local_date() {
        let timestamp = "2026-06-03T10:00:01Z";
        let expected = DateTime::parse_from_rfc3339(timestamp)
            .expect("timestamp")
            .with_timezone(&Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();

        assert_eq!(local_date_from_timestamp(timestamp), Some(expected));
    }

    #[test]
    fn rejects_out_of_range_unix_seconds() {
        assert_eq!(local_date_from_unix_seconds(u64::MAX), None);
    }
}
