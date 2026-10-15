//! The `dowm` command-line client: subcommands that talk to a running daemon over its
//! socket. Anything that is not a named subcommand is sent as a command line, so
//! `dowm workspace 3` and `dowm layout tiles horizontal` work exactly as in the config.

use std::io::{self, Write};
use std::path::Path;

use clap::{Subcommand, ValueEnum};
use dowm_client::{Client, ClientError};
use dowm_protocol::{MonitorInfo, Topic, WindowInfo, WorkspaceInfo};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TopicArg {
    Workspace,
    Focus,
    Mode,
}

impl From<TopicArg> for Topic {
    fn from(arg: TopicArg) -> Topic {
        match arg {
            TopicArg::Workspace => Topic::Workspace,
            TopicArg::Focus => Topic::Focus,
            TopicArg::Mode => Topic::Mode,
        }
    }
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum ClientCommand {
    /// List workspaces.
    ListWorkspaces {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// List windows.
    ListWindows {
        #[arg(long)]
        json: bool,
    },
    /// List monitors.
    ListMonitors {
        #[arg(long)]
        json: bool,
    },
    /// Print the active binding mode.
    Mode,
    /// Stream events as JSON lines until interrupted. No topics means all of them.
    Subscribe { topics: Vec<TopicArg> },
    /// Any other word is a command line for the daemon, e.g. `dowm workspace 3`.
    #[command(external_subcommand)]
    Run(Vec<String>),
}

fn json_line<T: serde::Serialize>(out: &mut impl Write, value: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(io::Error::other)?;
    writeln!(out)
}

/// Make a daemon-supplied string safe to print: control characters (terminal escapes,
/// tabs, newlines) and bidi overrides become spaces. App names, titles and bundle ids
/// are chosen by other programs, so none of them may steer the terminal or forge rows.
pub fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') { ' ' } else { c })
        .collect()
}

fn opt(value: &Option<String>) -> String { value.as_deref().map_or_else(|| "-".to_owned(), clean) }

fn flag(on: bool) -> &'static str { if on { "*" } else { " " } }

pub fn print_workspaces(out: &mut impl Write, workspaces: &[WorkspaceInfo]) -> io::Result<()> {
    for w in workspaces {
        writeln!(out, "{} {}\t{}\t{}\t{}", flag(w.focused), clean(&w.name), w.windows, if w.visible { "visible" } else { "hidden" }, opt(&w.monitor))?;
    }
    Ok(())
}

pub fn print_windows(out: &mut impl Write, windows: &[WindowInfo]) -> io::Result<()> {
    for w in windows {
        writeln!(out, "{} {}\t{}\t{}\t{}", flag(w.focused), w.id, opt(&w.app), opt(&w.workspace), clean(&w.title))?;
    }
    Ok(())
}

pub fn print_monitors(out: &mut impl Write, monitors: &[MonitorInfo]) -> io::Result<()> {
    for m in monitors {
        writeln!(out, "{} {}\t{}\t{}\t{}x{}", flag(m.focused), clean(&m.id), opt(&m.name), opt(&m.workspace), m.frame.width, m.frame.height)?;
    }
    Ok(())
}

/// Run `command` against the daemon listening on `socket`, writing results to `out`.
pub fn execute(command: ClientCommand, socket: &Path, out: &mut impl Write) -> Result<(), ClientError> {
    let mut client = Client::connect(socket)?;
    match command {
        ClientCommand::ListWorkspaces { json } => {
            let items = client.list_workspaces()?;
            if json { json_line(out, &items)? } else { print_workspaces(out, &items)? }
        }
        ClientCommand::ListWindows { json } => {
            let items = client.list_windows()?;
            if json { json_line(out, &items)? } else { print_windows(out, &items)? }
        }
        ClientCommand::ListMonitors { json } => {
            let items = client.list_monitors()?;
            if json { json_line(out, &items)? } else { print_monitors(out, &items)? }
        }
        ClientCommand::Mode => writeln!(out, "{}", clean(&client.mode()?))?,
        ClientCommand::Subscribe { topics } => {
            let topics: Vec<Topic> = topics.into_iter().map(Topic::from).collect();
            for event in client.subscribe(&topics)? {
                json_line(out, &event?)?;
                out.flush()?;
            }
        }
        ClientCommand::Run(words) => {
            client.command(&words.join(" "))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use dowm_protocol::Rect;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: ClientCommand,
    }

    fn parse(args: &[&str]) -> ClientCommand {
        Cli::try_parse_from(std::iter::once("dowm").chain(args.iter().copied())).unwrap().command
    }

    #[test]
    fn named_subcommands_parse() {
        assert_eq!(parse(&["list-workspaces"]), ClientCommand::ListWorkspaces { json: false });
        assert_eq!(parse(&["list-windows", "--json"]), ClientCommand::ListWindows { json: true });
        assert_eq!(parse(&["list-monitors"]), ClientCommand::ListMonitors { json: false });
        assert_eq!(parse(&["mode"]), ClientCommand::Mode);
        assert_eq!(
            parse(&["subscribe", "workspace", "mode"]),
            ClientCommand::Subscribe { topics: vec![TopicArg::Workspace, TopicArg::Mode] }
        );
        assert_eq!(parse(&["subscribe"]), ClientCommand::Subscribe { topics: vec![] });
    }

    #[test]
    fn everything_else_is_a_daemon_command_with_its_flags_intact() {
        assert_eq!(parse(&["workspace", "3"]), ClientCommand::Run(vec!["workspace".into(), "3".into()]));
        assert_eq!(
            parse(&["move-node-to-workspace", "--focus-follows-window", "2"]),
            ClientCommand::Run(vec!["move-node-to-workspace".into(), "--focus-follows-window".into(), "2".into()])
        );
        assert_eq!(
            parse(&["resize", "width", "-50"]),
            ClientCommand::Run(vec!["resize".into(), "width".into(), "-50".into()])
        );
    }

    #[test]
    fn an_unknown_topic_is_a_usage_error() {
        assert!(Cli::try_parse_from(["dowm", "subscribe", "nonsense"]).is_err());
    }

    #[test]
    fn tables_are_one_line_per_item_even_with_hostile_titles() {
        let mut out = Vec::new();
        print_windows(
            &mut out,
            &[WindowInfo {
                id: 5,
                pid: 1,
                app: None,
                title: "a\nb\tc\u{1b}[31m".into(),
                workspace: Some("2".into()),
                focused: true,
                frame: Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 },
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains('\u{1b}'));
        assert!(text.starts_with("* 5\t-\t2\t"));
    }

    #[test]
    fn every_printed_field_is_cleaned() {
        let mut out = Vec::new();
        print_windows(
            &mut out,
            &[WindowInfo {
                id: 1,
                pid: 1,
                app: Some("x\u{1b}]0;pwn\u{7}".into()),
                title: "t\u{202e}".into(),
                workspace: Some("w\nz".into()),
                focused: false,
                frame: Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 },
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.chars().any(|c| c == '\u{1b}' || c == '\u{7}' || c == '\u{202e}'), "{text:?}");
    }

    #[test]
    fn workspace_table_marks_focus_and_visibility() {
        let mut out = Vec::new();
        print_workspaces(
            &mut out,
            &[
                WorkspaceInfo { name: "1".into(), focused: true, visible: true, monitor: Some("M".into()), windows: 2 },
                WorkspaceInfo { name: "2".into(), focused: false, visible: false, monitor: None, windows: 0 },
            ],
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "* 1\t2\tvisible\tM\n  2\t0\thidden\t-\n");
    }
}
