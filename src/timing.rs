pub struct Timer {
    on: bool,
    start: std::time::Instant,
    last: std::time::Instant,
}

impl Timer {
    pub fn new() -> Timer {
        let now = std::time::Instant::now();
        Timer {
            on: std::env::var_os("KMX_TIMING").is_some(),
            start: now,
            last: now,
        }
    }

    pub fn mark(&mut self, what: &str) {
        if self.on {
            let now = std::time::Instant::now();
            let memory = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    let field = |k: &str| {
                        s.lines()
                            .find_map(|l| l.strip_prefix(k))
                            .map(|v| v.trim().to_string())
                    };
                    Some(format!(
                        "rss {}, peak {}",
                        field("VmRSS:")?,
                        field("VmHWM:")?
                    ))
                })
                .unwrap_or_default();
            eprintln!(
                "kmx: timing {what:<24} {:>8.1} ms (total {:.1} ms)  {memory}",
                (now - self.last).as_secs_f64() * 1e3,
                (now - self.start).as_secs_f64() * 1e3
            );
            self.last = now;
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}
