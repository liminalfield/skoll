//! Text for the Offset parameter: how it displays and how typed values are read.

/// `-250 ms` below a second, `12.500 s` below a minute, `1:02.500` above.
pub fn offset_to_string(seconds: f32) -> String {
    let abs = seconds.abs();
    let sign = if seconds < 0.0 { "-" } else { "" };
    if abs < 1.0 {
        format!("{} ms", (seconds * 1000.0).round())
    } else if abs < 60.0 {
        format!("{seconds:.3} s")
    } else {
        let total_ms = (f64::from(abs) * 1000.0).round() as u64;
        let (h, m, s, ms) = (
            total_ms / 3_600_000,
            total_ms / 60_000 % 60,
            total_ms / 1000 % 60,
            total_ms % 1000,
        );
        if h > 0 {
            format!("{sign}{h}:{m:02}:{s:02}.{ms:03}")
        } else {
            format!("{sign}{m}:{s:02}.{ms:03}")
        }
    }
}

/// Reads `250ms`, `1.5` or `1.5 s` (seconds), `1:02.5` (minutes:seconds) and `1:00:00`
/// (hours:minutes:seconds), each with an optional minus sign.
pub fn string_to_offset(text: &str) -> Option<f32> {
    let text = text.trim().to_ascii_lowercase();
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text.as_str()),
    };

    let seconds = if let Some(ms) = text.strip_suffix("ms") {
        ms.trim().parse::<f64>().ok()? / 1000.0
    } else if text.contains(':') {
        let parts: Vec<&str> = text.split(':').map(str::trim).collect();
        if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
            return None;
        }
        let (whole, last) = parts.split_at(parts.len() - 1);
        let mut total = 0.0;
        for part in whole {
            total = total * 60.0 + f64::from(part.parse::<u32>().ok()?);
        }
        total * 60.0 + last[0].parse::<f64>().ok().filter(|s| *s < 60.0)?
    } else {
        text.strip_suffix('s')
            .unwrap_or(text)
            .trim()
            .parse::<f64>()
            .ok()?
    };

    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let seconds = if negative { -seconds } else { seconds };
    Some(seconds as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_offsets() {
        assert_eq!(offset_to_string(0.0), "0 ms");
        assert_eq!(offset_to_string(-0.25), "-250 ms");
        assert_eq!(offset_to_string(0.999), "999 ms");
        assert_eq!(offset_to_string(12.5), "12.500 s");
        assert_eq!(offset_to_string(-1.0), "-1.000 s");
        assert_eq!(offset_to_string(62.5), "1:02.500");
        assert_eq!(offset_to_string(-125.25), "-2:05.250");
        assert_eq!(offset_to_string(3600.0), "1:00:00.000");
    }

    #[test]
    fn reads_typed_offsets() {
        assert_eq!(string_to_offset("250ms"), Some(0.25));
        assert_eq!(string_to_offset("-250 ms"), Some(-0.25));
        assert_eq!(string_to_offset("1.5"), Some(1.5));
        assert_eq!(string_to_offset(" 1.5 s "), Some(1.5));
        assert_eq!(string_to_offset("1:02.5"), Some(62.5));
        assert_eq!(string_to_offset("-1:02.5"), Some(-62.5));
        assert_eq!(string_to_offset("1:00:00"), Some(3600.0));
        assert_eq!(string_to_offset("0:00:01.25"), Some(1.25));
    }

    #[test]
    fn rejects_nonsense() {
        for text in ["", "abc", "1:", ":5", "1:75", "1:2:3:4", "--1", "inf", "1x"] {
            assert_eq!(string_to_offset(text), None, "{text:?}");
        }
    }

    #[test]
    fn displayed_values_read_back() {
        for value in [0.0, -0.25, 0.5, 12.5, -59.999, 62.5, -125.25, 3599.5] {
            let text = offset_to_string(value);
            let read = string_to_offset(&text).unwrap();
            assert!((read - value).abs() < 0.0006, "{value} -> {text} -> {read}");
        }
    }
}
