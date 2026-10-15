//! `[gaps]`: each of the six sizes is a number, or AeroSpace's per-monitor array
//! `[{ monitor.main = 16 }, { monitor."pattern" = 32 }, 24]` (first matching monitor
//! wins; the last element is the default).

use super::error::{Errors, join};
use super::monitor::parse_monitor_pattern;
use crate::layout::gaps::GapSizes;
use crate::model::display::{DisplayStore, DisplayUuid};
use crate::model::workspace::{MonitorPattern, Pattern};

/// One gap size: a default, and per-monitor overrides tried in order.
#[derive(Clone, Debug, Default)]
pub struct GapValue {
    overrides: Vec<(MonitorPattern, u32)>,
    /// `overrides`' patterns, compiled once here rather than on every layout. A pattern
    /// that does not compile (rejected at parse time) matches nothing.
    compiled: Vec<Option<Pattern>>,
    default: u32,
}

impl PartialEq for GapValue {
    fn eq(&self, other: &Self) -> bool { self.overrides == other.overrides && self.default == other.default }
}

impl Eq for GapValue {}

impl GapValue {
    pub fn constant(pixels: u32) -> Self { GapValue { overrides: Vec::new(), compiled: Vec::new(), default: pixels } }

    pub fn per_monitor(overrides: Vec<(MonitorPattern, u32)>, default: u32) -> Self {
        let compiled = overrides.iter().map(|(p, _)| Pattern::compile(p).ok()).collect();
        GapValue { overrides, compiled, default }
    }

    pub fn default_value(&self) -> u32 { self.default }

    pub fn overrides(&self) -> &[(MonitorPattern, u32)] { &self.overrides }

    /// The size on `display`: the first override whose pattern selects it, else the default.
    pub fn resolve(&self, display: &DisplayUuid, displays: &DisplayStore) -> u32 {
        self.overrides
            .iter()
            .zip(&self.compiled)
            .find(|(_, pattern)| pattern.as_ref().is_some_and(|p| p.matches(display, displays)))
            .map_or(self.default, |((_, pixels), _)| *pixels)
    }
}

/// The six gap sizes of a config.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gaps {
    pub inner_horizontal: GapValue,
    pub inner_vertical: GapValue,
    pub outer_left: GapValue,
    pub outer_right: GapValue,
    pub outer_top: GapValue,
    pub outer_bottom: GapValue,
}

impl Gaps {
    /// The sizes in force on `display`.
    pub fn resolve(&self, display: &DisplayUuid, displays: &DisplayStore) -> GapSizes {
        let at = |v: &GapValue| f64::from(v.resolve(display, displays));
        GapSizes {
            inner_horizontal: at(&self.inner_horizontal),
            inner_vertical: at(&self.inner_vertical),
            outer_left: at(&self.outer_left),
            outer_right: at(&self.outer_right),
            outer_top: at(&self.outer_top),
            outer_bottom: at(&self.outer_bottom),
        }
    }
}

/// Parse the `[gaps]` table found at `path`.
pub(crate) fn parse_gaps(table: &toml::Table, path: &str, errors: &mut Errors) -> Gaps {
    let mut gaps = Gaps::default();
    for (key, value) in table {
        let here = join(path, key);
        match key.as_str() {
            "inner" => {
                let Some(inner) = sub_table(value, &here, errors) else { continue };
                for (side, v) in inner {
                    let at = join(&here, side);
                    match side.as_str() {
                        "horizontal" => gaps.inner_horizontal = parse_value(v, &at, errors),
                        "vertical" => gaps.inner_vertical = parse_value(v, &at, errors),
                        _ => errors.push(at, "unknown key (expected horizontal or vertical)"),
                    }
                }
            }
            "outer" => {
                let Some(outer) = sub_table(value, &here, errors) else { continue };
                for (side, v) in outer {
                    let at = join(&here, side);
                    match side.as_str() {
                        "left" => gaps.outer_left = parse_value(v, &at, errors),
                        "right" => gaps.outer_right = parse_value(v, &at, errors),
                        "top" => gaps.outer_top = parse_value(v, &at, errors),
                        "bottom" => gaps.outer_bottom = parse_value(v, &at, errors),
                        _ => errors.push(at, "unknown key (expected left, right, top or bottom)"),
                    }
                }
            }
            _ => errors.push(here, "unknown key (expected inner or outer)"),
        }
    }
    gaps
}

fn sub_table<'a>(value: &'a toml::Value, path: &str, errors: &mut Errors) -> Option<&'a toml::Table> {
    let table = value.as_table();
    if table.is_none() {
        errors.push(path, "expected a table");
    }
    table
}

fn pixels(value: &toml::Value, path: &str, errors: &mut Errors) -> Option<u32> {
    match value.as_integer() {
        Some(n) => match u32::try_from(n) {
            Ok(n) => Some(n),
            Err(_) => {
                errors.push(path, format!("{n} is out of range (gaps are 0 or more pixels)"));
                None
            }
        },
        None => {
            errors.push(path, format!("expected an integer, found {}", value.type_str()));
            None
        }
    }
}

fn parse_value(value: &toml::Value, path: &str, errors: &mut Errors) -> GapValue {
    let Some(items) = value.as_array() else {
        return GapValue::constant(pixels(value, path, errors).unwrap_or(0));
    };
    let mut overrides = Vec::new();
    let mut default = None;
    for (i, item) in items.iter().enumerate() {
        let at = format!("{path}[{i}]");
        if item.is_integer() {
            if i + 1 != items.len() {
                errors.push(&at, "the default value must be the last element");
            }
            default = pixels(item, &at, errors).or(default);
            continue;
        }
        let Some(entry) = item.as_table() else {
            errors.push(&at, format!("expected an integer or {{ monitor.<pattern> = <int> }}, found {}", item.type_str()));
            continue;
        };
        let entries: Vec<(&String, &toml::Value)> = entry.iter().collect();
        let monitor = match entries.as_slice() {
            [(key, value)] if key.as_str() == "monitor" => value.as_table(),
            _ => None,
        };
        let Some(monitor) = monitor else {
            errors.push(&at, "expected { monitor.<pattern> = <int> }");
            continue;
        };
        let pairs: Vec<(&String, &toml::Value)> = monitor.iter().collect();
        let [(pattern, amount)] = pairs.as_slice() else {
            errors.push(&at, "`monitor` must hold exactly one pattern");
            continue;
        };
        let pattern_at = format!("{at}.monitor.{pattern}");
        let pattern = match parse_monitor_pattern(pattern.as_str()) {
            Ok(p) => Some(p),
            Err(why) => {
                errors.push(&pattern_at, why);
                None
            }
        };
        if let (Some(pattern), Some(amount)) = (pattern, pixels(amount, &pattern_at, errors)) {
            overrides.push((pattern, amount));
        }
    }
    if default.is_none() && !items.iter().any(|i| i.is_integer()) {
        errors.push(path, "a per-monitor list must end with a default integer");
    }
    GapValue::per_monitor(overrides, default.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::display::DisplaySnapshot;
    use crate::sys::screen::DisplayId;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn parse(text: &str) -> Result<Gaps, Vec<String>> {
        let table: toml::Table = toml::from_str(text).expect("valid toml");
        let mut errors = Errors::default();
        let gaps = parse_gaps(table.get("gaps").and_then(|g| g.as_table()).expect("gaps"), "gaps", &mut errors);
        errors.finish().map(|()| gaps).map_err(|e| e.iter().map(ToString::to_string).collect())
    }

    fn displays() -> DisplayStore {
        let mut store = DisplayStore::default();
        let snapshot = |uuid: &str, id, x, name: &str| DisplaySnapshot {
            uuid: DisplayUuid::new(uuid),
            cg_id: DisplayId::new(id),
            frame: CGRect::new(CGPoint::new(x, 0.), CGSize::new(1000., 800.)),
            name: Some(name.to_owned()),
        };
        store.apply(vec![snapshot("A", 1, 0., "Built-in Retina"), snapshot("B", 2, 1000., "DELL U2720Q")]);
        store
    }

    fn uuid(s: &str) -> DisplayUuid { DisplayUuid::new(s) }

    #[test]
    fn every_key_takes_an_integer() {
        let gaps = parse("[gaps]\ninner.horizontal = 1\ninner.vertical = 2\nouter.left = 3\nouter.right = 4\nouter.top = 5\nouter.bottom = 6\n").unwrap();
        for (value, expected) in [
            (&gaps.inner_horizontal, 1),
            (&gaps.inner_vertical, 2),
            (&gaps.outer_left, 3),
            (&gaps.outer_right, 4),
            (&gaps.outer_top, 5),
            (&gaps.outer_bottom, 6),
        ] {
            assert_eq!(*value, GapValue::constant(expected));
        }
    }

    #[test]
    fn omitted_keys_default_to_zero_and_the_aerospace_spacing_style_parses() {
        let gaps = parse("[gaps]\ninner.horizontal =0 \ninner.vertical =  0 \nouter.left =      0 \n").unwrap();
        assert_eq!(gaps, Gaps::default());
        assert!(parse("[gaps]\n").unwrap() == Gaps::default());
    }

    #[test]
    fn the_per_monitor_array_form_parses_with_a_default() {
        let gaps = parse("[gaps]\nouter.top = [{ monitor.main = 16 }, { monitor.\"DELL.*\" = 32 }, { monitor.2 = 8 }, 24]\n").unwrap();
        assert_eq!(
            gaps.outer_top,
            GapValue::per_monitor(
                vec![
                    (MonitorPattern::Main, 16),
                    (MonitorPattern::Name("DELL.*".into()), 32),
                    (MonitorPattern::Sequence(2), 8),
                ],
                24
            )
        );
        let secondary = parse("[gaps]\nouter.top = [{ monitor.secondary = 4 }, 0]\n").unwrap();
        assert_eq!(secondary.outer_top.overrides(), [(MonitorPattern::Secondary, 4)]);
    }

    #[test]
    fn a_bare_array_default_is_a_constant() {
        let gaps = parse("[gaps]\ninner.vertical = [7]\n").unwrap();
        assert_eq!(gaps.inner_vertical, GapValue::constant(7));
    }

    #[test]
    fn resolution_picks_the_first_matching_monitor_else_the_default() {
        let gaps = parse("[gaps]\nouter.top = [{ monitor.main = 16 }, { monitor.\"DELL\" = 32 }, 24]\n").unwrap();
        let d = displays();
        assert_eq!(gaps.outer_top.resolve(&uuid("A"), &d), 16, "main");
        assert_eq!(gaps.outer_top.resolve(&uuid("B"), &d), 32, "name regex");
        assert_eq!(gaps.outer_top.resolve(&uuid("Z"), &d), 24, "no match: the default");
        assert_eq!(gaps.resolve(&uuid("A"), &d).outer_top, 16.0);
        assert_eq!(gaps.resolve(&uuid("B"), &d).outer_top, 32.0);
        assert_eq!(gaps.resolve(&uuid("B"), &d).outer_left, 0.0);
    }

    #[test]
    fn the_first_match_wins_when_several_patterns_select_the_display() {
        let gaps = parse("[gaps]\nouter.left = [{ monitor.1 = 5 }, { monitor.main = 9 }, 0]\n").unwrap();
        assert_eq!(gaps.outer_left.resolve(&uuid("A"), &displays()), 5);
    }

    #[test]
    fn errors_carry_the_key_path() {
        let e = parse("[gaps]\ninner.horizontal = -1\n").unwrap_err();
        assert!(e[0].starts_with("gaps.inner.horizontal:"), "{e:?}");
        let e = parse("[gaps]\nouter.top = \"big\"\n").unwrap_err();
        assert!(e[0].starts_with("gaps.outer.top:") && e[0].contains("string"), "{e:?}");
        let e = parse("[gaps]\ninner.diagonal = 1\n").unwrap_err();
        assert!(e[0].starts_with("gaps.inner.diagonal:"), "{e:?}");
        let e = parse("[gaps]\nmiddle.left = 1\n").unwrap_err();
        assert!(e[0].starts_with("gaps.middle:"), "{e:?}");
        let e = parse("[gaps]\ninner = 3\n").unwrap_err();
        assert!(e[0].starts_with("gaps.inner:") && e[0].contains("table"), "{e:?}");
    }

    #[test]
    fn per_monitor_list_errors() {
        let e = parse("[gaps]\nouter.top = [{ monitor.main = 1 }]\n").unwrap_err();
        assert!(e.iter().any(|m| m.starts_with("gaps.outer.top:") && m.contains("default")), "{e:?}");
        let e = parse("[gaps]\nouter.top = [5, { monitor.main = 1 }]\n").unwrap_err();
        assert!(e.iter().any(|m| m.contains("last element")), "{e:?}");
        let e = parse("[gaps]\nouter.top = [{ monitor.\"(\" = 1 }, 0]\n").unwrap_err();
        assert!(e[0].starts_with("gaps.outer.top[0].monitor.(:") && e[0].contains("regex"), "{e:?}");
        let e = parse("[gaps]\nouter.top = [{ monitor.0 = 1 }, 0]\n").unwrap_err();
        assert!(e[0].contains("start at 1"), "{e:?}");
        let e = parse("[gaps]\nouter.top = [{ monitor.main = 1, monitor.secondary = 2 }, 0]\n").unwrap_err();
        assert!(e[0].contains("exactly one"), "{e:?}");
        let e = parse("[gaps]\nouter.top = [{ screen.main = 1 }, 0]\n").unwrap_err();
        assert!(e[0].contains("monitor.<pattern>"), "{e:?}");
        let e = parse("[gaps]\nouter.top = [{ monitor.main = \"x\" }, 0]\n").unwrap_err();
        assert!(e[0].starts_with("gaps.outer.top[0].monitor.main:"), "{e:?}");
        let e = parse("[gaps]\nouter.top = [\"x\", 0]\n").unwrap_err();
        assert!(e[0].starts_with("gaps.outer.top[0]:"), "{e:?}");
        let e = parse("[gaps]\nouter.top = [-4]\n").unwrap_err();
        assert!(e[0].contains("out of range"), "{e:?}");
    }

    #[test]
    fn all_errors_are_reported_not_just_the_first() {
        let e = parse("[gaps]\ninner.horizontal = -1\nouter.left = \"x\"\nouter.top = true\n").unwrap_err();
        assert_eq!(e.len(), 3, "{e:?}");
    }
}
