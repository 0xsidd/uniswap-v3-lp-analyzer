use std::io::Write;
use std::time::Instant;

pub fn format_duration(ms: u128) -> String {
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    let s = (ms / 1000) as u64;
    if s < 60 {
        return format!("{}s", s);
    }
    let m = s / 60;
    let rem = s % 60;
    if m < 60 {
        return format!("{}m {}s", m, rem);
    }
    let h = m / 60;
    format!("{}h {}m", h, m % 60)
}

pub fn render(_label: &str, processed: u64, total: u64, extra: &str, start: Instant) {
    let pct = processed as f64 / total as f64;
    let bar_width = 40;
    let filled = (pct * bar_width as f64).round() as usize;
    let bar: String = "\u{2588}".repeat(filled) + &"\u{2591}".repeat(bar_width - filled);
    let elapsed_ms = start.elapsed().as_millis();
    let eta = if pct > 0.0001 && elapsed_ms > 2000 {
        format_duration(((elapsed_ms as f64 / pct) * (1.0 - pct)).round() as u128)
    } else {
        "calculating...".to_string()
    };
    let extra_str = if extra.is_empty() {
        String::new()
    } else {
        format!("  {}", extra)
    };
    print!(
        "\r\x1b[2K  {} {:.1}%  |  {}/{}{}  |  elapsed: {}  |  ETA: {}",
        bar,
        pct * 100.0,
        processed,
        total,
        extra_str,
        format_duration(elapsed_ms),
        eta
    );
    std::io::stdout().flush().ok();
}
