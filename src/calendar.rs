use std::time::{SystemTime, UNIX_EPOCH};

pub fn date_from_timestamp(timestamp: &str) -> Option<&str> {
    let date = timestamp.get(0..10)?;
    (date.as_bytes().get(4) == Some(&b'-') && date.as_bytes().get(7) == Some(&b'-')).then_some(date)
}

pub fn date_from_system_time(time: SystemTime) -> Option<String> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(date_string_from_days((seconds / 86_400) as i64))
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
    fn parses_codex_timestamp_date() {
        assert_eq!(
            date_from_timestamp("2026-06-03T10:00:01Z"),
            Some("2026-06-03")
        );
    }
}
