//! `[[on-window-detected]]` rules.
//!
//! When a window first appears, the rules are tried in file order. The first rule whose
//! `if` conditions all hold runs; later rules are skipped unless that rule set
//! `check-further-callbacks = true`, in which case evaluation continues with the next
//! one (as AeroSpace documents).
//!
//! `if.app-id` is an exact bundle id. The two regex matchers search anywhere in the
//! text ("substring") and are case-insensitive.

use regex::{Regex, RegexBuilder};

use super::command::Action;
use super::error::join;
use super::schema::Ctx;

/// A compiled regex that remembers its source, so rules can be compared.
#[derive(Clone, Debug)]
pub struct Matcher {
    source: String,
    regex: Regex,
}

impl Matcher {
    pub fn new(source: &str) -> Result<Self, regex::Error> {
        let regex = RegexBuilder::new(source).case_insensitive(true).build()?;
        Ok(Matcher { source: source.to_owned(), regex })
    }

    pub fn source(&self) -> &str { &self.source }

    pub fn is_match(&self, text: &str) -> bool { self.regex.is_match(text) }
}

impl PartialEq for Matcher {
    fn eq(&self, other: &Self) -> bool { self.source == other.source }
}

impl Eq for Matcher {}

/// What the rules can see of a newly detected window.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowFacts<'a> {
    /// The app's bundle id.
    pub app_id: Option<&'a str>,
    pub app_name: Option<&'a str>,
    pub title: &'a str,
    /// The name of the workspace the window landed on.
    pub workspace: Option<&'a str>,
    /// dowm is still in its start-up phase.
    pub during_startup: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowRule {
    pub app_id: Option<String>,
    pub app_name: Option<Matcher>,
    pub title: Option<Matcher>,
    pub workspace: Option<String>,
    pub during_startup: Option<bool>,
    pub check_further_callbacks: bool,
    pub run: Vec<Action>,
}

impl WindowRule {
    /// Whether every condition of this rule holds. A rule with no conditions matches
    /// every window. A condition on a fact the window lacks (no bundle id, say) fails.
    pub fn matches(&self, facts: &WindowFacts<'_>) -> bool {
        self.app_id.as_deref().is_none_or(|id| facts.app_id == Some(id))
            && self.app_name.as_ref().is_none_or(|m| facts.app_name.is_some_and(|n| m.is_match(n)))
            && self.title.as_ref().is_none_or(|m| m.is_match(facts.title))
            && self.workspace.as_deref().is_none_or(|w| facts.workspace == Some(w))
            && self.during_startup.is_none_or(|s| s == facts.during_startup)
    }
}

/// The rules that run for `facts`, in order: the first match, then (while the previous
/// one asked for it) the following matches.
pub fn rules_to_run<'a>(rules: &'a [WindowRule], facts: &WindowFacts<'_>) -> Vec<&'a WindowRule> {
    let mut out = Vec::new();
    for rule in rules.iter().filter(|r| r.matches(facts)) {
        out.push(rule);
        if !rule.check_further_callbacks {
            break;
        }
    }
    out
}

/// Parse `on-window-detected` (an array of tables) found at `path`.
pub(crate) fn parse_rules(value: &toml::Value, path: &str, ctx: &mut Ctx) -> Vec<WindowRule> {
    let Some(items) = value.as_array() else {
        ctx.errors.push(path, format!("expected an array of tables ([[{path}]]), found {}", value.type_str()));
        return Vec::new();
    };
    let mut rules = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let at = format!("{path}[{i}]");
        let Some(table) = item.as_table() else {
            ctx.errors.push(at, format!("expected a table, found {}", item.type_str()));
            continue;
        };
        rules.push(parse_rule(table, &at, ctx));
    }
    rules
}

fn parse_rule(table: &toml::Table, path: &str, ctx: &mut Ctx) -> WindowRule {
    let mut rule = WindowRule::default();
    let mut run = None;
    for (key, value) in table {
        let at = join(path, key);
        match key.as_str() {
            "if" => match value.as_table() {
                Some(conditions) => parse_conditions(conditions, &at, &mut rule, ctx),
                None => ctx.errors.push(at, format!("expected a table, found {}", value.type_str())),
            },
            "check-further-callbacks" => {
                if let Some(b) = ctx.bool(value, &at) {
                    rule.check_further_callbacks = b;
                }
            }
            "run" => run = Some(ctx.actions(value, &at)),
            _ => ctx.errors.push(at, "unknown key (expected if, check-further-callbacks or run)"),
        }
    }
    match run {
        Some(run) => rule.run = run,
        None => ctx.errors.push(join(path, "run"), "missing key `run`"),
    }
    rule
}

fn parse_conditions(table: &toml::Table, path: &str, rule: &mut WindowRule, ctx: &mut Ctx) {
    for (key, value) in table {
        let at = join(path, key);
        match key.as_str() {
            "app-id" => rule.app_id = ctx.string(value, &at).map(str::to_owned),
            "workspace" => rule.workspace = ctx.string(value, &at).map(str::to_owned),
            "during-aerospace-startup" => rule.during_startup = ctx.bool(value, &at),
            "app-name-regex-substring" => rule.app_name = regex_condition(value, &at, ctx),
            "window-title-regex-substring" => rule.title = regex_condition(value, &at, ctx),
            _ => ctx.errors.push(
                at,
                "unknown key (expected app-id, app-name-regex-substring, window-title-regex-substring, workspace or during-aerospace-startup)",
            ),
        }
    }
    if let Some(workspace) = &rule.workspace {
        ctx.workspace_refs.insert(workspace.clone());
    }
}

fn regex_condition(value: &toml::Value, path: &str, ctx: &mut Ctx) -> Option<Matcher> {
    let source = ctx.string(value, path)?;
    match Matcher::new(source) {
        Ok(m) => Some(m),
        Err(e) => {
            ctx.errors.push(path, format!("bad regex `{source}`: {e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::reactor::Command;

    fn parse(text: &str) -> (Vec<WindowRule>, Result<(), Vec<String>>, Ctx) {
        let table: toml::Table = toml::from_str(text).expect("valid toml");
        let mut ctx = Ctx::default();
        let rules = table
            .get("on-window-detected")
            .map(|v| parse_rules(v, "on-window-detected", &mut ctx))
            .unwrap_or_default();
        let errors = std::mem::take(&mut ctx.errors).finish().map_err(|e| e.iter().map(ToString::to_string).collect());
        (rules, errors, ctx)
    }

    fn rule(conditions: &str, extra: &str) -> WindowRule {
        let (rules, errors, _) =
            parse(&format!("[[on-window-detected]]\n{conditions}\n{extra}\nrun = 'fullscreen'\n"));
        errors.expect("valid rule");
        rules.into_iter().next().expect("one rule")
    }

    fn facts() -> WindowFacts<'static> {
        WindowFacts {
            app_id: Some("com.github.wez.wezterm"),
            app_name: Some("WezTerm"),
            title: "~/code — zsh",
            workspace: Some("2"),
            during_startup: false,
        }
    }

    #[test]
    fn app_id_is_an_exact_match() {
        let r = rule("if.app-id = 'com.github.wez.wezterm'", "");
        assert!(r.matches(&facts()));
        assert!(!r.matches(&WindowFacts { app_id: Some("com.github.wez.wezterm2"), ..facts() }));
        assert!(!r.matches(&WindowFacts { app_id: Some("com.github.wez"), ..facts() }));
        assert!(!r.matches(&WindowFacts { app_id: None, ..facts() }), "no bundle id: no match");
    }

    #[test]
    fn app_name_is_a_case_insensitive_regex_substring() {
        let r = rule("if.app-name-regex-substring = 'wez'", "");
        assert!(r.matches(&facts()));
        assert!(rule("if.app-name-regex-substring = '^Wez.*m$'", "").matches(&facts()));
        assert!(!rule("if.app-name-regex-substring = '^Term'", "").matches(&facts()));
        assert!(!r.matches(&WindowFacts { app_name: None, ..facts() }));
        assert!(!r.matches(&WindowFacts { app_name: Some("Safari"), ..facts() }));
    }

    #[test]
    fn title_is_a_case_insensitive_regex_substring() {
        let r = rule("if.window-title-regex-substring = 'ZSH$'", "");
        assert!(r.matches(&facts()));
        assert!(!r.matches(&WindowFacts { title: "vim", ..facts() }));
        assert!(rule("if.window-title-regex-substring = '^$'", "").matches(&WindowFacts { title: "", ..facts() }));
    }

    #[test]
    fn workspace_condition_compares_the_workspace_name() {
        let r = rule("if.workspace = '2'", "");
        assert!(r.matches(&facts()));
        assert!(!r.matches(&WindowFacts { workspace: Some("3"), ..facts() }));
        assert!(!r.matches(&WindowFacts { workspace: None, ..facts() }));
    }

    #[test]
    fn startup_condition_distinguishes_startup_from_later() {
        let at_startup = rule("if.during-aerospace-startup = true", "");
        let later = rule("if.during-aerospace-startup = false", "");
        let during = WindowFacts { during_startup: true, ..facts() };
        assert!(at_startup.matches(&during) && !at_startup.matches(&facts()));
        assert!(later.matches(&facts()) && !later.matches(&during));
        assert!(rule("if.app-id = 'com.github.wez.wezterm'", "").matches(&during), "unspecified: always");
    }

    #[test]
    fn conditions_combine_with_and() {
        let r = rule("if.app-id = 'com.github.wez.wezterm'\nif.window-title-regex-substring = 'vim'", "");
        assert!(!r.matches(&facts()), "title does not match");
        assert!(r.matches(&WindowFacts { title: "vim", ..facts() }));
    }

    #[test]
    fn a_rule_without_conditions_matches_everything() {
        let (rules, errors, _) = parse("[[on-window-detected]]\nrun = 'layout floating'\n");
        errors.unwrap();
        assert!(rules[0].matches(&facts()));
        assert!(rules[0].matches(&WindowFacts::default()));
    }

    #[test]
    fn the_first_match_wins_unless_it_asks_to_check_further() {
        let (rules, errors, _) = parse(
            "[[on-window-detected]]\nif.app-id = 'a'\nrun = 'fullscreen'\n\
             [[on-window-detected]]\nif.app-id = 'a'\nrun = 'balance-sizes'\n",
        );
        errors.unwrap();
        let a = WindowFacts { app_id: Some("a"), ..Default::default() };
        let hits = rules_to_run(&rules, &a);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].run, vec![Action::Reactor(Command::Fullscreen)]);

        let (rules, errors, _) = parse(
            "[[on-window-detected]]\nif.app-id = 'a'\ncheck-further-callbacks = true\nrun = 'fullscreen'\n\
             [[on-window-detected]]\nif.app-id = 'b'\nrun = 'balance-sizes'\n\
             [[on-window-detected]]\nif.app-id = 'a'\nrun = 'balance-sizes'\n",
        );
        errors.unwrap();
        let hits = rules_to_run(&rules, &a);
        assert_eq!(hits.len(), 2, "continues past the non-matching rule to the next match, then stops");
        assert_eq!(hits[1].run, vec![Action::Reactor(Command::BalanceSizes)]);
        assert!(rules_to_run(&rules, &WindowFacts { app_id: Some("zzz"), ..Default::default() }).is_empty());
    }

    #[test]
    fn run_accepts_a_string_or_an_array_and_records_workspace_references() {
        let (rules, errors, ctx) = parse(
            "[[on-window-detected]]\nif.app-id = 'x'\nrun = ['move-node-to-workspace T', 'layout floating']\n\
             [[on-window-detected]]\nif.workspace = 'W'\nrun = 'move-node-to-workspace B'\n",
        );
        errors.unwrap();
        assert_eq!(rules[0].run.len(), 2);
        assert_eq!(ctx.workspace_refs.iter().cloned().collect::<Vec<_>>(), ["B", "T", "W"]);
    }

    #[test]
    fn check_further_callbacks_defaults_to_false() {
        assert!(!rule("if.app-id = 'x'", "").check_further_callbacks);
        assert!(rule("if.app-id = 'x'", "check-further-callbacks = true").check_further_callbacks);
    }

    #[test]
    fn errors_carry_the_rule_index_and_key() {
        let (_, errors, _) = parse(
            "[[on-window-detected]]\nif.app-id = 'x'\nrun = 'fullscreen'\n\
             [[on-window-detected]]\nif.window-title-regex-substring = '('\nrun = 'fullscreen'\n",
        );
        let e = errors.unwrap_err();
        assert!(e[0].starts_with("on-window-detected[1].if.window-title-regex-substring:"), "{e:?}");

        let (_, errors, _) = parse("[[on-window-detected]]\nif.app-name-regex-substring = '[a'\nrun = 'fullscreen'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].if.app-name-regex-substring:"));

        let (_, errors, _) = parse("[[on-window-detected]]\nif.app-id = 'x'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].run:"));

        let (_, errors, _) = parse("[[on-window-detected]]\nif.colour = 'x'\nrun = 'fullscreen'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].if.colour:"));

        let (_, errors, _) = parse("[[on-window-detected]]\nwhen = 'x'\nrun = 'fullscreen'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].when:"));

        let (_, errors, _) = parse("[[on-window-detected]]\nif.app-id = 3\nrun = 'fullscreen'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].if.app-id:"));

        let (_, errors, _) = parse("[[on-window-detected]]\ncheck-further-callbacks = 'yes'\nrun = 'fullscreen'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].check-further-callbacks:"));

        let (_, errors, _) = parse("[[on-window-detected]]\nif.app-id = 'x'\nrun = 'nonsense'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].run:"));

        let (_, errors, _) = parse("[[on-window-detected]]\nif = 'x'\nrun = 'fullscreen'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].if:"));

        let (_, errors, _) = parse("[[on-window-detected]]\nif.during-aerospace-startup = 'x'\nrun = 'fullscreen'\n");
        assert!(errors.unwrap_err()[0].starts_with("on-window-detected[0].if.during-aerospace-startup:"));
    }

    #[test]
    fn a_non_array_on_window_detected_is_an_error() {
        let table: toml::Table = toml::from_str("on-window-detected = 3").unwrap();
        let mut ctx = Ctx::default();
        parse_rules(&table["on-window-detected"], "on-window-detected", &mut ctx);
        assert!(ctx.errors.finish().unwrap_err().mentions("on-window-detected"));

        let table: toml::Table = toml::from_str("on-window-detected = [1]").unwrap();
        let mut ctx = Ctx::default();
        parse_rules(&table["on-window-detected"], "on-window-detected", &mut ctx);
        assert!(ctx.errors.finish().unwrap_err().mentions("on-window-detected[0]"));
    }
}
