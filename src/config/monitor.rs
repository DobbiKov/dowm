//! AeroSpace's `MonitorDescription`: a sequence number, `main`, `secondary`, or a regex
//! searched in the monitor's name.

use regex::Regex;

use crate::model::workspace::MonitorPattern;

/// Parse a monitor description, checking that a number is at least 1 and that a name
/// pattern compiles.
pub fn parse_monitor_pattern(text: &str) -> Result<MonitorPattern, String> {
    match text {
        "main" => return Ok(MonitorPattern::Main),
        "secondary" => return Ok(MonitorPattern::Secondary),
        "" => return Err("empty monitor description".into()),
        _ => {}
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        return match text.parse::<usize>() {
            Ok(0) => Err("monitor sequence numbers start at 1".into()),
            Ok(n) => Ok(MonitorPattern::Sequence(n)),
            Err(e) => Err(format!("bad monitor sequence number `{text}`: {e}")),
        };
    }
    Regex::new(text).map_err(|e| format!("bad monitor name regex `{text}`: {e}"))?;
    Ok(MonitorPattern::Name(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_description_form() {
        assert_eq!(parse_monitor_pattern("main"), Ok(MonitorPattern::Main));
        assert_eq!(parse_monitor_pattern("secondary"), Ok(MonitorPattern::Secondary));
        assert_eq!(parse_monitor_pattern("1"), Ok(MonitorPattern::Sequence(1)));
        assert_eq!(parse_monitor_pattern("12"), Ok(MonitorPattern::Sequence(12)));
        assert_eq!(parse_monitor_pattern("DELL.*"), Ok(MonitorPattern::Name("DELL.*".into())));
        assert_eq!(parse_monitor_pattern("Built-in"), Ok(MonitorPattern::Name("Built-in".into())));
    }

    #[test]
    fn rejects_empty_zero_overflowing_and_bad_regex() {
        assert!(parse_monitor_pattern("").is_err());
        assert!(parse_monitor_pattern("0").unwrap_err().contains("start at 1"));
        assert!(parse_monitor_pattern("99999999999999999999999").is_err());
        assert!(parse_monitor_pattern("(unclosed").unwrap_err().contains("regex"));
    }
}
