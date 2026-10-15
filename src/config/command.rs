//! Command strings (`workspace 3`, `layout tiles horizontal`, `mode resize`) parsed into
//! typed [`Action`]s.
//!
//! Every command in the config goes through [`parse_command`] when the config loads, so a
//! typo is a config error rather than something that fails when the key is pressed.
//! Most commands map onto the reactor's [`Command`]; the ones that act outside the
//! window tree (`mode`, `reload-config`, `exec-and-forget`) are their own variants and
//! become outbound effects.

use std::fmt;

use super::monitor::parse_monitor_pattern;
use crate::actor::reactor::{Command, MouseTarget};
use crate::layout::ops::{LayoutArg, ResizeDimension, SplitArg};
use crate::model::workspace::{Direction, MonitorPattern, MonitorTarget};

/// What a binding or callback does.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// A command for the reactor.
    Reactor(Command),
    /// `mode <name>`: switch the active binding mode.
    Mode(String),
    /// `reload-config`.
    ReloadConfig,
    /// `exec-and-forget <shell command>`: run it detached.
    ExecAndForget(String),
}

impl Action {
    /// Key auto-repeat re-runs the binding only for commands where holding the key makes
    /// sense: `resize`.
    pub fn is_repeatable(&self) -> bool {
        matches!(self, Action::Reactor(Command::Resize { .. }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandParseError {
    Empty,
    UnknownCommand(String),
    MissingArgument { command: &'static str, expected: &'static str },
    UnexpectedArgument { command: &'static str, argument: String },
    UnknownFlag { command: &'static str, flag: String },
    BadValue { command: &'static str, value: String, expected: &'static str },
    BadMonitor { command: &'static str, why: String },
    BadNumber { command: &'static str, value: String },
}

impl fmt::Display for CommandParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty command"),
            Self::UnknownCommand(c) => write!(f, "unknown command `{c}`"),
            Self::MissingArgument { command, expected } => {
                write!(f, "`{command}` needs an argument: {expected}")
            }
            Self::UnexpectedArgument { command, argument } => {
                write!(f, "`{command}` does not take the argument `{argument}`")
            }
            Self::UnknownFlag { command, flag } => {
                write!(f, "`{command}` does not support the flag `{flag}`")
            }
            Self::BadValue { command, value, expected } => {
                write!(f, "`{command}`: bad value `{value}`, expected {expected}")
            }
            Self::BadMonitor { command, why } => write!(f, "`{command}`: {why}"),
            Self::BadNumber { command, value } => {
                write!(f, "`{command}`: `{value}` is not a whole number of pixels")
            }
        }
    }
}

impl std::error::Error for CommandParseError {}

type Result<T> = std::result::Result<T, CommandParseError>;

/// Flags and positional arguments of one command.
struct Args<'a> {
    command: &'static str,
    flags: Vec<&'a str>,
    positional: Vec<&'a str>,
}

impl<'a> Args<'a> {
    fn new(command: &'static str, words: &[&'a str]) -> Self {
        let (flags, positional) = words.iter().partition(|w| w.starts_with("--"));
        Args { command, flags, positional }
    }

    /// Consume `flag` if present.
    fn flag(&mut self, flag: &str) -> bool {
        let before = self.flags.len();
        self.flags.retain(|f| *f != flag);
        self.flags.len() != before
    }

    /// Fail on any flag nobody consumed.
    fn no_more_flags(&self) -> Result<()> {
        match self.flags.first() {
            Some(flag) => Err(CommandParseError::UnknownFlag {
                command: self.command,
                flag: (*flag).to_owned(),
            }),
            None => Ok(()),
        }
    }

    /// Exactly one positional argument.
    fn one(&self, expected: &'static str) -> Result<&'a str> {
        self.no_more_flags()?;
        match self.positional.as_slice() {
            [] => Err(CommandParseError::MissingArgument { command: self.command, expected }),
            [one] => Ok(one),
            [_, extra, ..] => Err(CommandParseError::UnexpectedArgument {
                command: self.command,
                argument: (*extra).to_owned(),
            }),
        }
    }

    /// No arguments at all.
    fn none(&self) -> Result<()> {
        self.no_more_flags()?;
        match self.positional.first() {
            Some(extra) => Err(CommandParseError::UnexpectedArgument {
                command: self.command,
                argument: (*extra).to_owned(),
            }),
            None => Ok(()),
        }
    }

    fn bad(&self, value: &str, expected: &'static str) -> CommandParseError {
        CommandParseError::BadValue { command: self.command, value: value.to_owned(), expected }
    }
}

fn direction(args: &Args<'_>, text: &str) -> Result<Direction> {
    parse_direction(text).ok_or_else(|| args.bad(text, "left, down, up or right"))
}

fn parse_direction(text: &str) -> Option<Direction> {
    Some(match text {
        "left" => Direction::Left,
        "down" => Direction::Down,
        "up" => Direction::Up,
        "right" => Direction::Right,
        _ => return None,
    })
}

/// Parse one command string.
pub fn parse_command(text: &str) -> Result<Action> {
    let text = text.trim();
    let (name, rest) = match text.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (text, ""),
    };
    if name.is_empty() {
        return Err(CommandParseError::Empty);
    }
    // The shell command is everything after the name, untouched.
    if name == "exec-and-forget" {
        return if rest.is_empty() {
            Err(CommandParseError::MissingArgument {
                command: "exec-and-forget",
                expected: "a shell command",
            })
        } else {
            Ok(Action::ExecAndForget(rest.to_owned()))
        };
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    let reactor = |c| Ok(Action::Reactor(c));
    match name {
        "workspace" => {
            let args = Args::new("workspace", &words);
            let ws = args.one("a workspace name")?;
            reactor(Command::Workspace(workspace_name(&args, ws)?))
        }
        "move-node-to-workspace" => {
            let mut args = Args::new("move-node-to-workspace", &words);
            let follow = args.flag("--focus-follows-window");
            let ws = args.one("a workspace name")?;
            reactor(Command::MoveNodeToWorkspace {
                workspace: workspace_name(&args, ws)?,
                window: None,
                follow,
            })
        }
        "workspace-back-and-forth" => {
            Args::new("workspace-back-and-forth", &words).none()?;
            reactor(Command::WorkspaceBackAndForth)
        }
        "move-workspace-to-monitor" => {
            let mut args = Args::new("move-workspace-to-monitor", &words);
            let wrap = args.flag("--wrap-around");
            let target = args.one("next, prev or a monitor description")?;
            reactor(Command::MoveWorkspaceToMonitor(match target {
                "next" => MonitorTarget::Next { wrap },
                "prev" => MonitorTarget::Prev { wrap },
                other => MonitorTarget::Pattern(monitor(&args, other)?),
            }))
        }
        "focus-monitor" => {
            let mut args = Args::new("focus-monitor", &words);
            let wrap = args.flag("--wrap-around");
            let target = args.one("a direction, next, prev or a monitor description")?;
            reactor(Command::FocusMonitor(match target {
                "next" => MonitorTarget::Next { wrap },
                "prev" => MonitorTarget::Prev { wrap },
                other => match parse_direction(other) {
                    Some(direction) => MonitorTarget::Direction { direction, wrap },
                    None => MonitorTarget::Pattern(monitor(&args, other)?),
                },
            }))
        }
        "focus" => parse_focus(&words),
        "move" => {
            let args = Args::new("move", &words);
            let d = args.one("left, down, up or right")?;
            reactor(Command::Move(direction(&args, d)?))
        }
        "join-with" => {
            let args = Args::new("join-with", &words);
            let d = args.one("left, down, up or right")?;
            reactor(Command::JoinWith(direction(&args, d)?))
        }
        "split" => {
            let args = Args::new("split", &words);
            reactor(Command::Split(match args.one("horizontal, vertical or opposite")? {
                "horizontal" => SplitArg::Horizontal,
                "vertical" => SplitArg::Vertical,
                "opposite" => SplitArg::Opposite,
                other => return Err(args.bad(other, "horizontal, vertical or opposite")),
            }))
        }
        "resize" => parse_resize(&words),
        "layout" => {
            let args = Args::new("layout", &words);
            args.no_more_flags()?;
            if args.positional.is_empty() {
                return Err(CommandParseError::MissingArgument {
                    command: "layout",
                    expected: "one or more layouts",
                });
            }
            let layouts = args
                .positional
                .iter()
                .map(|w| layout_arg(&args, w))
                .collect::<Result<Vec<_>>>()?;
            reactor(Command::Layout(layouts))
        }
        "fullscreen" => {
            Args::new("fullscreen", &words).none()?;
            reactor(Command::Fullscreen)
        }
        "balance-sizes" => {
            Args::new("balance-sizes", &words).none()?;
            reactor(Command::BalanceSizes)
        }
        "flatten-workspace-tree" => {
            Args::new("flatten-workspace-tree", &words).none()?;
            reactor(Command::FlattenWorkspaceTree)
        }
        "close-all-windows-but-current" => {
            Args::new("close-all-windows-but-current", &words).none()?;
            reactor(Command::CloseAllWindowsButCurrent)
        }
        "reload-config" => {
            Args::new("reload-config", &words).none()?;
            Ok(Action::ReloadConfig)
        }
        "mode" => {
            let args = Args::new("mode", &words);
            Ok(Action::Mode(args.one("a binding mode name")?.to_owned()))
        }
        "move-mouse" => {
            let args = Args::new("move-mouse", &words);
            let (target, lazy) = match args.one("window-lazy-center, monitor-lazy-center, ...")? {
                "window-lazy-center" => (MouseTarget::Window, true),
                "window-force-center" => (MouseTarget::Window, false),
                "monitor-lazy-center" => (MouseTarget::Monitor, true),
                "monitor-force-center" => (MouseTarget::Monitor, false),
                other => {
                    return Err(args.bad(
                        other,
                        "window-lazy-center, window-force-center, monitor-lazy-center or monitor-force-center",
                    ));
                }
            };
            reactor(Command::MoveMouse { target, lazy })
        }
        other => Err(CommandParseError::UnknownCommand(other.to_owned())),
    }
}

/// `workspace next|prev` exist in AeroSpace but are not supported here; refusing them
/// keeps them from silently becoming workspace names.
fn workspace_name(args: &Args<'_>, name: &str) -> Result<String> {
    if matches!(name, "next" | "prev") {
        return Err(args.bad(name, "a workspace name (`next` and `prev` are not supported)"));
    }
    Ok(name.to_owned())
}

fn monitor(args: &Args<'_>, text: &str) -> Result<MonitorPattern> {
    parse_monitor_pattern(text)
        .map_err(|why| CommandParseError::BadMonitor { command: args.command, why })
}

fn layout_arg(args: &Args<'_>, text: &str) -> Result<LayoutArg> {
    Ok(match text {
        "tiles" => LayoutArg::Tiles,
        "accordion" => LayoutArg::Accordion,
        "floating" => LayoutArg::Floating,
        "tiling" => LayoutArg::Tiling,
        "horizontal" => LayoutArg::Horizontal,
        "vertical" => LayoutArg::Vertical,
        "h_tiles" => LayoutArg::HTiles,
        "v_tiles" => LayoutArg::VTiles,
        "h_accordion" => LayoutArg::HAccordion,
        "v_accordion" => LayoutArg::VAccordion,
        other => {
            return Err(args.bad(
                other,
                "tiles, accordion, floating, tiling, horizontal, vertical, h_tiles, v_tiles, h_accordion or v_accordion",
            ));
        }
    })
}

/// `focus [--boundaries workspace] [--boundaries-action stop|fail|wrap-around-the-workspace]
/// <direction>`.
fn parse_focus(words: &[&str]) -> Result<Action> {
    const COMMAND: &str = "focus";
    let mut wrap = false;
    let mut direction_word = None;
    let mut iter = words.iter().copied();
    while let Some(word) = iter.next() {
        match word {
            "--boundaries" => match iter.next() {
                Some("workspace") => {}
                Some(other) => {
                    return Err(CommandParseError::BadValue {
                        command: COMMAND,
                        value: other.to_owned(),
                        expected: "workspace",
                    });
                }
                None => {
                    return Err(CommandParseError::MissingArgument {
                        command: COMMAND,
                        expected: "a boundary after --boundaries",
                    });
                }
            },
            "--boundaries-action" => match iter.next() {
                Some("wrap-around-the-workspace") => wrap = true,
                Some("stop" | "fail") => wrap = false,
                Some(other) => {
                    return Err(CommandParseError::BadValue {
                        command: COMMAND,
                        value: other.to_owned(),
                        expected: "stop, fail or wrap-around-the-workspace",
                    });
                }
                None => {
                    return Err(CommandParseError::MissingArgument {
                        command: COMMAND,
                        expected: "an action after --boundaries-action",
                    });
                }
            },
            flag if flag.starts_with("--") => {
                return Err(CommandParseError::UnknownFlag { command: COMMAND, flag: flag.to_owned() });
            }
            positional => {
                if direction_word.replace(positional).is_some() {
                    return Err(CommandParseError::UnexpectedArgument {
                        command: COMMAND,
                        argument: positional.to_owned(),
                    });
                }
            }
        }
    }
    let args = Args::new(COMMAND, &[]);
    let word = direction_word.ok_or(CommandParseError::MissingArgument {
        command: COMMAND,
        expected: "left, down, up or right",
    })?;
    Ok(Action::Reactor(Command::Focus { direction: direction(&args, word)?, wrap }))
}

/// `resize width|height|smart (+N|-N|N)`.
fn parse_resize(words: &[&str]) -> Result<Action> {
    let args = Args::new("resize", words);
    args.no_more_flags()?;
    let (dimension, amount) = match args.positional.as_slice() {
        [] | [_] => {
            return Err(CommandParseError::MissingArgument {
                command: "resize",
                expected: "width|height|smart and an amount",
            });
        }
        [d, a] => (*d, *a),
        [_, _, extra, ..] => {
            return Err(CommandParseError::UnexpectedArgument {
                command: "resize",
                argument: (*extra).to_owned(),
            });
        }
    };
    let dimension = match dimension {
        "width" => ResizeDimension::Width,
        "height" => ResizeDimension::Height,
        "smart" => ResizeDimension::Smart,
        other => return Err(args.bad(other, "width, height or smart")),
    };
    let number = |text: &str| {
        text.parse::<i32>()
            .map_err(|_| CommandParseError::BadNumber { command: "resize", value: text.to_owned() })
    };
    // A signed amount is a delta; a bare number is the new size.
    if amount.starts_with(['+', '-']) {
        return Ok(Action::Reactor(Command::Resize { dimension, amount: number(amount)? }));
    }
    let size = number(amount)?;
    if dimension == ResizeDimension::Smart {
        return Err(args.bad(
            amount,
            "a signed change (+N or -N); an exact size works for width and height only",
        ));
    }
    Ok(Action::Reactor(Command::ResizeTo { dimension, size }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Action { parse_command(text).unwrap_or_else(|e| panic!("{text:?}: {e}")) }

    fn cmd(c: Command) -> Action { Action::Reactor(c) }

    fn err(text: &str) -> CommandParseError {
        parse_command(text).expect_err(&format!("{text:?} should not parse"))
    }

    #[test]
    fn workspace_commands() {
        assert_eq!(ok("workspace 3"), cmd(Command::Workspace("3".into())));
        assert_eq!(ok("workspace T"), cmd(Command::Workspace("T".into())));
        assert_eq!(ok("  workspace   B  "), cmd(Command::Workspace("B".into())));
        assert_eq!(
            ok("move-node-to-workspace 5"),
            cmd(Command::MoveNodeToWorkspace { workspace: "5".into(), window: None, follow: false })
        );
        assert_eq!(
            ok("move-node-to-workspace --focus-follows-window T"),
            cmd(Command::MoveNodeToWorkspace { workspace: "T".into(), window: None, follow: true })
        );
        assert_eq!(
            ok("move-node-to-workspace T --focus-follows-window"),
            cmd(Command::MoveNodeToWorkspace { workspace: "T".into(), window: None, follow: true })
        );
        assert_eq!(ok("workspace-back-and-forth"), cmd(Command::WorkspaceBackAndForth));
    }

    #[test]
    fn monitor_commands() {
        let mv = |t| cmd(Command::MoveWorkspaceToMonitor(t));
        assert_eq!(ok("move-workspace-to-monitor next"), mv(MonitorTarget::Next { wrap: false }));
        assert_eq!(ok("move-workspace-to-monitor --wrap-around next"), mv(MonitorTarget::Next { wrap: true }));
        assert_eq!(ok("move-workspace-to-monitor --wrap-around prev"), mv(MonitorTarget::Prev { wrap: true }));
        assert_eq!(ok("move-workspace-to-monitor prev"), mv(MonitorTarget::Prev { wrap: false }));
        assert_eq!(ok("move-workspace-to-monitor main"), mv(MonitorTarget::Pattern(MonitorPattern::Main)));
        assert_eq!(ok("move-workspace-to-monitor secondary"), mv(MonitorTarget::Pattern(MonitorPattern::Secondary)));
        assert_eq!(ok("move-workspace-to-monitor 2"), mv(MonitorTarget::Pattern(MonitorPattern::Sequence(2))));
        assert_eq!(ok("move-workspace-to-monitor DELL"), mv(MonitorTarget::Pattern(MonitorPattern::Name("DELL".into()))));

        let fm = |t| cmd(Command::FocusMonitor(t));
        assert_eq!(ok("focus-monitor next"), fm(MonitorTarget::Next { wrap: false }));
        assert_eq!(ok("focus-monitor --wrap-around prev"), fm(MonitorTarget::Prev { wrap: true }));
        assert_eq!(ok("focus-monitor left"), fm(MonitorTarget::Direction { direction: Direction::Left, wrap: false }));
        assert_eq!(ok("focus-monitor --wrap-around right"), fm(MonitorTarget::Direction { direction: Direction::Right, wrap: true }));
        assert_eq!(ok("focus-monitor main"), fm(MonitorTarget::Pattern(MonitorPattern::Main)));
        assert_eq!(ok("focus-monitor 1"), fm(MonitorTarget::Pattern(MonitorPattern::Sequence(1))));
    }

    #[test]
    fn focus_move_and_join_take_a_direction() {
        for (word, d) in [("left", Direction::Left), ("down", Direction::Down), ("up", Direction::Up), ("right", Direction::Right)] {
            assert_eq!(ok(&format!("focus {word}")), cmd(Command::Focus { direction: d, wrap: false }));
            assert_eq!(ok(&format!("move {word}")), cmd(Command::Move(d)));
            assert_eq!(ok(&format!("join-with {word}")), cmd(Command::JoinWith(d)));
        }
    }

    #[test]
    fn focus_boundary_flags() {
        let wrap = cmd(Command::Focus { direction: Direction::Left, wrap: true });
        assert_eq!(ok("focus --boundaries-action wrap-around-the-workspace left"), wrap);
        assert_eq!(ok("focus left --boundaries-action wrap-around-the-workspace"), wrap);
        assert_eq!(ok("focus --boundaries workspace --boundaries-action wrap-around-the-workspace left"), wrap);
        for stop in ["stop", "fail"] {
            assert_eq!(
                ok(&format!("focus --boundaries-action {stop} up")),
                cmd(Command::Focus { direction: Direction::Up, wrap: false })
            );
        }
    }

    #[test]
    fn split_forms() {
        assert_eq!(ok("split horizontal"), cmd(Command::Split(SplitArg::Horizontal)));
        assert_eq!(ok("split vertical"), cmd(Command::Split(SplitArg::Vertical)));
        assert_eq!(ok("split opposite"), cmd(Command::Split(SplitArg::Opposite)));
    }

    #[test]
    fn resize_forms() {
        let r = |dimension, amount| cmd(Command::Resize { dimension, amount });
        assert_eq!(ok("resize width +50"), r(ResizeDimension::Width, 50));
        assert_eq!(ok("resize width -50"), r(ResizeDimension::Width, -50));
        assert_eq!(ok("resize height +1"), r(ResizeDimension::Height, 1));
        assert_eq!(ok("resize height -10"), r(ResizeDimension::Height, -10));
        assert_eq!(ok("resize smart +50"), r(ResizeDimension::Smart, 50));
        assert_eq!(ok("resize smart -50"), r(ResizeDimension::Smart, -50));
        assert_eq!(ok("resize width 800"), cmd(Command::ResizeTo { dimension: ResizeDimension::Width, size: 800 }));
        assert_eq!(ok("resize height 0"), cmd(Command::ResizeTo { dimension: ResizeDimension::Height, size: 0 }));
    }

    #[test]
    fn layout_forms_cover_every_argument() {
        let l = |args: Vec<LayoutArg>| cmd(Command::Layout(args));
        for (word, arg) in [
            ("tiles", LayoutArg::Tiles),
            ("accordion", LayoutArg::Accordion),
            ("floating", LayoutArg::Floating),
            ("tiling", LayoutArg::Tiling),
            ("horizontal", LayoutArg::Horizontal),
            ("vertical", LayoutArg::Vertical),
            ("h_tiles", LayoutArg::HTiles),
            ("v_tiles", LayoutArg::VTiles),
            ("h_accordion", LayoutArg::HAccordion),
            ("v_accordion", LayoutArg::VAccordion),
        ] {
            assert_eq!(ok(&format!("layout {word}")), l(vec![arg]), "{word}");
        }
        assert_eq!(ok("layout tiles horizontal vertical"), l(vec![LayoutArg::Tiles, LayoutArg::Horizontal, LayoutArg::Vertical]));
        assert_eq!(ok("layout accordion horizontal vertical"), l(vec![LayoutArg::Accordion, LayoutArg::Horizontal, LayoutArg::Vertical]));
        assert_eq!(ok("layout floating tiling"), l(vec![LayoutArg::Floating, LayoutArg::Tiling]));
    }

    #[test]
    fn argumentless_commands() {
        assert_eq!(ok("fullscreen"), cmd(Command::Fullscreen));
        assert_eq!(ok("balance-sizes"), cmd(Command::BalanceSizes));
        assert_eq!(ok("flatten-workspace-tree"), cmd(Command::FlattenWorkspaceTree));
        assert_eq!(ok("close-all-windows-but-current"), cmd(Command::CloseAllWindowsButCurrent));
        assert_eq!(ok("reload-config"), Action::ReloadConfig);
    }

    #[test]
    fn meta_commands() {
        assert_eq!(ok("mode resize"), Action::Mode("resize".into()));
        assert_eq!(ok("mode main"), Action::Mode("main".into()));
        assert_eq!(
            ok("exec-and-forget open -n /System/Applications/Utilities/Terminal.app"),
            Action::ExecAndForget("open -n /System/Applications/Utilities/Terminal.app".into())
        );
        assert_eq!(
            ok("exec-and-forget   echo 'a  b' | tee   /tmp/x"),
            Action::ExecAndForget("echo 'a  b' | tee   /tmp/x".into()),
            "the shell command is passed through untouched"
        );
    }

    #[test]
    fn move_mouse_targets() {
        let m = |target, lazy| cmd(Command::MoveMouse { target, lazy });
        assert_eq!(ok("move-mouse window-lazy-center"), m(MouseTarget::Window, true));
        assert_eq!(ok("move-mouse monitor-lazy-center"), m(MouseTarget::Monitor, true));
        assert_eq!(ok("move-mouse window-force-center"), m(MouseTarget::Window, false));
        assert_eq!(ok("move-mouse monitor-force-center"), m(MouseTarget::Monitor, false));
    }

    #[test]
    fn rejects_empty_and_unknown_commands() {
        assert_eq!(err(""), CommandParseError::Empty);
        assert_eq!(err("   "), CommandParseError::Empty);
        assert_eq!(err("workspac 3"), CommandParseError::UnknownCommand("workspac".into()));
        assert_eq!(err("sticky"), CommandParseError::UnknownCommand("sticky".into()));
    }

    #[test]
    fn rejects_missing_extra_and_bad_arguments() {
        for bad in [
            "workspace", "workspace 1 2", "workspace next", "workspace prev", "workspace --bogus 1",
            "move-node-to-workspace", "move-node-to-workspace --nope 1", "move-node-to-workspace a b",
            "workspace-back-and-forth now", "move-workspace-to-monitor", "move-workspace-to-monitor --bogus next",
            "move-workspace-to-monitor 0", "move-workspace-to-monitor (bad", "focus-monitor", "focus-monitor (bad",
            "focus", "focus sideways", "focus left right", "focus --boundaries-action", "focus --boundaries-action explode left",
            "focus --boundaries", "focus --boundaries cosmos left", "focus --ignore-floating left",
            "move", "move diagonal", "join-with", "join-with x", "split", "split diagonal",
            "resize", "resize width", "resize width +5 +6", "resize depth +5", "resize width +x", "resize width 5x",
            "resize smart 100", "resize width --flag +5", "resize width +99999999999", "resize width -",
            "layout", "layout sticky", "layout tiles --flag", "fullscreen on", "balance-sizes now", "flatten-workspace-tree x",
            "close-all-windows-but-current x", "reload-config now", "mode", "mode a b", "exec-and-forget", "exec-and-forget   ",
            "move-mouse", "move-mouse center", "move-mouse window-lazy-center x",
        ] {
            assert!(parse_command(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn errors_name_the_command_and_the_problem() {
        let e = err("resize width +x").to_string();
        assert!(e.contains("resize") && e.contains("+x"), "{e}");
        let e = err("focus sideways").to_string();
        assert!(e.contains("focus") && e.contains("sideways"), "{e}");
        let e = err("frobnicate").to_string();
        assert!(e.contains("frobnicate"), "{e}");
    }

    #[test]
    fn only_resize_repeats() {
        assert!(ok("resize width +50").is_repeatable());
        assert!(!ok("focus left").is_repeatable());
        assert!(!ok("mode main").is_repeatable());
        assert!(!ok("workspace 1").is_repeatable());
    }

    #[test]
    fn arbitrary_strings_never_panic() {
        for junk in ["\0", "💥 workspace", "workspace \u{200b}1", "layout \t\n x", "resize width +", "resize width -", "focus --", "mode \u{0}", "--", "-- --"] {
            let _ = parse_command(junk);
        }
    }
}
