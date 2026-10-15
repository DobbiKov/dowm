//! Config errors, collected rather than thrown: a config with five mistakes reports all
//! five, each with the key path it was found at.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// Dotted key path (`mode.main.binding.alt-h`); empty for a file-level problem such
    /// as a TOML syntax error.
    pub path: String,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Everything wrong with a config. Never empty when returned as an `Err`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl ConfigErrors {
    pub fn iter(&self) -> impl Iterator<Item = &ConfigError> { self.0.iter() }

    pub fn len(&self) -> usize { self.0.len() }

    pub fn is_empty(&self) -> bool { self.0.is_empty() }

    /// Whether some error was reported at `path` or below it.
    pub fn mentions(&self, path: &str) -> bool {
        self.0.iter().any(|e| e.path == path || e.path.starts_with(&format!("{path}.")))
    }
}

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, error) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

/// The collector the parsers push into.
#[derive(Debug, Default)]
pub struct Errors(Vec<ConfigError>);

impl Errors {
    pub fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(ConfigError { path: path.into(), message: message.into() });
    }

    pub fn is_empty(&self) -> bool { self.0.is_empty() }

    pub fn finish(self) -> Result<(), ConfigErrors> {
        if self.0.is_empty() { Ok(()) } else { Err(ConfigErrors(self.0)) }
    }
}

/// `parent.child`, or `child` at the top level.
pub fn join(parent: &str, child: &str) -> String {
    if parent.is_empty() { child.to_owned() } else { format!("{parent}.{child}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_display_one_line_each_with_their_path() {
        let mut errors = Errors::default();
        errors.push("gaps.inner.vertical", "must not be negative");
        errors.push("", "TOML syntax error");
        let err = errors.finish().unwrap_err();
        assert_eq!(err.to_string(), "gaps.inner.vertical: must not be negative\nTOML syntax error");
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn mentions_matches_a_path_and_its_children_only() {
        let mut errors = Errors::default();
        errors.push("mode.main.binding.alt-h", "x");
        let err = errors.finish().unwrap_err();
        assert!(err.mentions("mode.main.binding.alt-h"));
        assert!(err.mentions("mode.main"));
        assert!(err.mentions("mode"));
        assert!(!err.mentions("mode.ma"));
        assert!(!err.mentions("gaps"));
    }

    #[test]
    fn no_errors_finish_ok() {
        assert!(Errors::default().finish().is_ok());
    }

    #[test]
    fn join_skips_an_empty_parent() {
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a", "b"), "a.b");
    }
}
