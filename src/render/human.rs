use std::path::Path;

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

pub fn local_time(secs: u64) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return secs.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

pub fn ago(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    if d < 2 {
        return "just now".to_string();
    }
    let (n, unit) = if d < 60 {
        (d, "second")
    } else if d < 3600 {
        (d / 60, "minute")
    } else if d < 48 * 3600 {
        (d / 3600, "hour")
    } else if d < 14 * 86400 {
        (d / 86400, "day")
    } else if d < 60 * 86400 {
        (d / (7 * 86400), "week")
    } else if d < 730 * 86400 {
        (d / (30 * 86400), "month")
    } else {
        (d / (365 * 86400), "year")
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

pub fn tilde(p: &Path) -> String {
    match std::env::home_dir().filter(|h| !h.as_os_str().is_empty()) {
        Some(home) => match p.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        },
        None => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        let now = 10_000_000;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 45, now), "45 seconds ago");
        assert_eq!(ago(now - 60, now), "1 minute ago");
        assert_eq!(ago(now - 3 * 3600, now), "3 hours ago");
        assert_eq!(ago(now - 2 * 86400, now), "2 days ago");
        assert_eq!(ago(now - 35 * 86400, now), "5 weeks ago");
        assert_eq!(ago(now - 90 * 86400, now), "3 months ago");
        assert_eq!(ago(0, 3 * 365 * 86400), "3 years ago");
    }
}
