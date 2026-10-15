//! Helpers shared by the config parsers: the error sink plus the references that can only
//! be checked once the whole file is read (which modes and workspaces exist).

use std::collections::BTreeSet;

use super::command::{Action, parse_command};
use super::error::Errors;
use crate::actor::reactor::Command;

/// State threaded through parsing one config file.
#[derive(Debug, Default)]
pub(crate) struct Ctx {
    pub errors: Errors,
    /// `(key path, mode name)` for every `mode <name>` command, checked against the
    /// declared modes at the end.
    pub mode_refs: Vec<(String, String)>,
    /// Workspace names mentioned by commands, rules and assignments: the pool's extras.
    pub workspace_refs: BTreeSet<String>,
}

impl Ctx {
    /// One command string, parsed and its references recorded. Errors go to the sink.
    pub fn command(&mut self, text: &str, path: &str) -> Option<Action> {
        match parse_command(text) {
            Ok(action) => {
                match &action {
                    Action::Mode(name) => self.mode_refs.push((path.to_owned(), name.clone())),
                    Action::Reactor(Command::Workspace(name))
                    | Action::Reactor(Command::MoveNodeToWorkspace { workspace: name, .. }) => {
                        self.workspace_refs.insert(name.clone());
                    }
                    _ => {}
                }
                Some(action)
            }
            Err(e) => {
                self.errors.push(path, e.to_string());
                None
            }
        }
    }

    /// A command string or an array of them (`run`, a binding's value, a callback).
    pub fn actions(&mut self, value: &toml::Value, path: &str) -> Vec<Action> {
        match value {
            toml::Value::String(text) => self.command(text, path).into_iter().collect(),
            toml::Value::Array(items) => items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let at = format!("{path}[{i}]");
                    match item.as_str() {
                        Some(text) => self.command(text, &at),
                        None => {
                            self.errors.push(
                                at,
                                format!("expected a command string, found {}", item.type_str()),
                            );
                            None
                        }
                    }
                })
                .collect(),
            other => {
                self.errors.push(
                    path,
                    format!("expected a command or an array of commands, found {}", other.type_str()),
                );
                Vec::new()
            }
        }
    }

    pub fn bool(&mut self, value: &toml::Value, path: &str) -> Option<bool> {
        let b = value.as_bool();
        if b.is_none() {
            self.errors.push(path, format!("expected a boolean, found {}", value.type_str()));
        }
        b
    }

    pub fn string<'a>(&mut self, value: &'a toml::Value, path: &str) -> Option<&'a str> {
        let s = value.as_str();
        if s.is_none() {
            self.errors.push(path, format!("expected a string, found {}", value.type_str()));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> toml::Value {
        toml::from_str::<toml::Table>(&format!("v = {text}")).expect("toml").remove("v").expect("v")
    }

    #[test]
    fn a_string_is_one_action_and_an_array_runs_in_order() {
        let mut ctx = Ctx::default();
        assert_eq!(ctx.actions(&value("'fullscreen'"), "k").len(), 1);
        let many = ctx.actions(&value("['reload-config', 'mode main', 'workspace 2']"), "k");
        assert_eq!(many, vec![
            Action::ReloadConfig,
            Action::Mode("main".into()),
            Action::Reactor(Command::Workspace("2".into())),
        ]);
        assert!(ctx.errors.is_empty());
        assert_eq!(ctx.mode_refs, vec![("k[1]".to_owned(), "main".to_owned())]);
        assert!(ctx.workspace_refs.contains("2"));
    }

    #[test]
    fn an_empty_array_is_no_actions() {
        let mut ctx = Ctx::default();
        assert!(ctx.actions(&value("[]"), "k").is_empty());
        assert!(ctx.errors.is_empty());
    }

    #[test]
    fn bad_commands_and_wrong_types_are_reported_with_their_index() {
        let mut ctx = Ctx::default();
        let actions = ctx.actions(&value("['fullscreen', 'bogus', 3]"), "run");
        assert_eq!(actions.len(), 1, "the good one is kept");
        let err = ctx.errors.finish().unwrap_err();
        assert!(err.mentions("run[1]") && err.mentions("run[2]"), "{err}");

        let mut ctx = Ctx::default();
        ctx.actions(&value("5"), "run");
        assert!(ctx.errors.finish().unwrap_err().mentions("run"));
    }

    #[test]
    fn workspace_references_are_collected_from_both_workspace_commands() {
        let mut ctx = Ctx::default();
        ctx.actions(&value("['workspace T', 'move-node-to-workspace --focus-follows-window B']"), "k");
        assert_eq!(ctx.workspace_refs.iter().cloned().collect::<Vec<_>>(), ["B", "T"]);
    }
}
