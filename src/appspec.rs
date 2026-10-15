//! `--app <name|pid>` parsing and name matching (pure).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppSpec {
    Pid(i32),
    Name(String),
}

impl AppSpec {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("app name or pid must not be empty".into());
        }
        if s.chars().all(|c| c.is_ascii_digit()) {
            return s.parse::<i32>().map(AppSpec::Pid).map_err(|_| format!("pid out of range: {s}"));
        }
        Ok(AppSpec::Name(s.to_string()))
    }
}

/// Pick the best candidate index for `query`: exact (case-insensitive) match first,
/// otherwise a unique substring match. `Err` carries the ambiguous/none explanation.
pub fn pick_by_name(query: &str, candidates: &[String]) -> Result<usize, String> {
    let q = query.to_lowercase();
    if let Some(i) = candidates.iter().position(|c| c.to_lowercase() == q) {
        return Ok(i);
    }
    let hits: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => Err(format!("no running app matches '{query}'")),
        many => Err(format!(
            "'{query}' is ambiguous: {}",
            many.iter().map(|i| candidates[*i].as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Parse `10,20,40` into a sorted, de-duplicated list of positive counts.
pub fn parse_counts(s: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let n: usize = part.trim().parse().map_err(|_| format!("invalid window count '{}'", part.trim()))?;
        if n == 0 {
            return Err("window counts must be >= 1".into());
        }
        out.push(n);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pid_and_name() {
        assert_eq!(AppSpec::parse("1234"), Ok(AppSpec::Pid(1234)));
        assert_eq!(AppSpec::parse(" Safari "), Ok(AppSpec::Name("Safari".into())));
        assert_eq!(AppSpec::parse("1password"), Ok(AppSpec::Name("1password".into())));
    }

    #[test]
    fn parse_rejects_empty_and_overflow() {
        assert!(AppSpec::parse("").is_err());
        assert!(AppSpec::parse("   ").is_err());
        assert!(AppSpec::parse("99999999999999").is_err());
    }

    fn names() -> Vec<String> {
        ["Safari", "Safari Technology Preview", "Finder", "WezTerm"].map(String::from).to_vec()
    }

    #[test]
    fn exact_match_beats_substring() {
        assert_eq!(pick_by_name("safari", &names()), Ok(0));
    }

    #[test]
    fn unique_substring_matches() {
        assert_eq!(pick_by_name("wez", &names()), Ok(3));
    }

    #[test]
    fn ambiguous_and_missing_report_errors() {
        let e = pick_by_name("saf", &names()).unwrap_err();
        assert!(e.contains("ambiguous") && e.contains("Safari Technology Preview"));
        assert!(pick_by_name("nope", &names()).unwrap_err().contains("no running app"));
        assert!(pick_by_name("x", &[]).is_err());
    }

    #[test]
    fn counts_parse_sort_dedup() {
        assert_eq!(parse_counts("40, 10,20,10"), Ok(vec![10, 20, 40]));
        assert_eq!(parse_counts("5"), Ok(vec![5]));
    }

    #[test]
    fn counts_reject_bad_input() {
        assert!(parse_counts("").is_err());
        assert!(parse_counts("10,,20").is_err());
        assert!(parse_counts("0").is_err());
        assert!(parse_counts("-3").is_err());
        assert!(parse_counts("a").is_err());
    }
}
