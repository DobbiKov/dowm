//! The dowm daemon.
//!
//! Tracks every window and display, runs the workspace and tiling commands, and (M4)
//! reads the config: hotkeys and binding modes, `on-window-detected` rules, callbacks,
//! gaps, focus-follows-mouse, with hot reload.
//!
//! Threads: the reactor runs on its own thread; each app's AX actor on another (spawned
//! by the WM controller); CGS notifications are translated on a dispatcher thread; the
//! config watcher polls the file on its own thread. The main thread runs AppKit plus the
//! WM controller, NSWorkspace/display notifications, CGS registration, the event taps
//! (hotkeys, pointer) and the effect runner.

use std::path::PathBuf;

use clap::Parser;
use dowm::cli::ClientCommand;

#[derive(Parser)]
#[command(
    name = "dowm",
    about = "dowm window manager: run it with no subcommand to start the daemon, or give a subcommand (or any dowm command) to control a running one",
    // `dowm workspace 3`: the word after a flag-less name is a command, not a typo.
    subcommand_required = false,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Log filter (tracing `EnvFilter` syntax). Defaults to `RUST_LOG`, else `dowm=info`.
    #[arg(long)]
    log: Option<String>,
    /// Config file. Default: ~/.config/dowm/dowm.toml, else ~/.dowm.toml, else the
    /// built-in default.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// The daemon's socket. Default: `$DOWM_SOCKET`, else `$TMPDIR/dowm-<uid>/dowm.sock`.
    #[arg(long, value_name = "PATH", global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<ClientCommand>,
}

fn main() {
    let cli = Cli::parse();
    let socket = cli.socket.clone().unwrap_or_else(dowm_client::default_socket_path);
    match cli.command {
        Some(command) => client(command, &socket),
        None => run_daemon(cli, socket),
    }
}

/// Talk to a running daemon; works on any OS the socket is reachable from.
fn client(command: ClientCommand, socket: &std::path::Path) {
    let mut stdout = std::io::stdout().lock();
    match dowm::cli::execute(command, socket, &mut stdout) {
        Ok(()) => {}
        // `dowm subscribe | head` closing the pipe is not an error worth printing.
        Err(dowm_client::ClientError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => {
            eprintln!("dowm: {}", dowm::cli::clean(&e.to_string()));
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn run_daemon(_cli: Cli, _socket: PathBuf) {
    eprintln!("the dowm daemon only runs on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn run_daemon(cli: Cli, socket: PathBuf) { daemon::main(cli, socket) }

#[cfg(target_os = "macos")]
mod daemon {
    use super::PathBuf;
    use dowm::actor;
    use dowm::actor::notification_center::NotificationCenter;
    use dowm::actor::input::watcher::{self, POLL_INTERVAL};
    use dowm::actor::input::{Shared, keeper, runner};
    use dowm::actor::reactor::animation::{AnimationManager, AnimationSender};
    use dowm::actor::reactor::{Event, Reactor};
    use dowm::config::watch::WatchState;
    use dowm::config::{self, ConfigSource};
    use dowm::actor::menubar;
    use dowm::ipc::{self, EventBus, ReactorBackend};
    use dowm_protocol::Topic;
    use dowm::actor::window_notify::{WINDOW_EVENTS, WindowNotify};
    use dowm::actor::wm_controller::WmController;
    use dowm::platform::ax_restorer::AxRestorer;
    use dowm::restore::SharedRegistry;
    use dowm::sys::executor::Executor;
    use dowm::termination::{self, TerminationConfig, Terminator};
    use std::sync::Arc;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use tracing::{error, info};
    use tracing_subscriber::EnvFilter;

    /// How long after launch the start-up phase lasts (`if.during-aerospace-startup`,
    /// `after-startup-command`): long enough for the running apps to report their windows.
    const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

    pub fn main(cli: super::Cli, socket: PathBuf) {
        init_logging(cli.log.as_deref());
        install_panic_hook();

        let source = config::resolve_source(
            cli.config.as_deref(),
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
            &|path| path.is_file(),
        );
        let (initial, initial_text) = match config::load_text(&source) {
            Ok((config, text)) => (Arc::new(config), text),
            Err(error) => {
                error!("cannot start: the config is invalid\n{error}");
                std::process::exit(1);
            }
        };
        match source.path() {
            Some(path) => info!(path = %path.display(), "config loaded"),
            None => info!("no config file found; using the built-in default"),
        }

        if !dowm::platform::ax::is_trusted() {
            error!(
                "accessibility access is not granted; enable dowm (or your terminal) in \
                 System Settings > Privacy & Security > Accessibility"
            );
            std::process::exit(1);
        }

        let mtm = MainThreadMarker::new().expect("dowm must start on the main thread");
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let (events_tx, events_rx) = actor::channel();
        let (notify_tx, notify_rx) = actor::channel();

        // Every window the reactor parks is recorded here; the handler installed below
        // puts them all back on SIGTERM/INT/HUP/QUIT, on a panic and on a normal exit.
        let registry = Arc::new(SharedRegistry::new());
        let terminator =
            Terminator::new(Arc::clone(&registry), Arc::new(AxRestorer::system(Arc::clone(&registry))), TerminationConfig::default());
        let _termination_guard =
            termination::install(terminator).expect("failed to install termination handlers");

        let (presenter_tx, presenter_rx) = AnimationSender::channel();
        std::thread::Builder::new()
            .name("presenter".into())
            .spawn(move || AnimationManager::run(presenter_rx))
            .expect("failed to spawn presenter thread");

        // Change events fan out from the reactor (workspace, focus) and the effect runner
        // (mode) to IPC subscribers and the menu bar.
        let bus = Arc::new(EventBus::new());
        let (menu_tx, menu_rx) = actor::channel();
        bus.subscribe(&[Topic::Workspace, Topic::Mode], move |event| menu_tx.try_send(event).is_ok());

        let mut reactor = match Reactor::with_config(Some(notify_tx), Arc::clone(&initial)) {
            Ok(reactor) => reactor,
            Err(error) => {
                error!(%error, "cannot start: the workspace pool is invalid");
                std::process::exit(1);
            }
        };
        let (effects_tx, effects_rx) = actor::channel();
        reactor.set_effect_sender(effects_tx);
        reactor.set_parked_registry(registry);
        reactor.set_presenter(presenter_tx);
        reactor.set_event_bus(Arc::clone(&bus));
        reactor.set_foreign_probe(dowm::sys::native_spaces::system_probe());
        std::thread::Builder::new()
            .name("reactor".into())
            .spawn(move || Executor::run(reactor.run(events_rx)))
            .expect("failed to spawn reactor thread");

        // Time-based bookkeeping (unhide confirmation TTL) must run even when no event does.
        let tick_tx = events_tx.clone();
        std::thread::Builder::new()
            .name("visibility-tick".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    tick_tx.send(dowm::actor::reactor::Event::VisibilityTick);
                }
            })
            .expect("failed to spawn tick thread");

        let tick_tx = events_tx.clone();
        std::thread::Builder::new()
            .name("startup".into())
            .spawn(move || {
                std::thread::sleep(STARTUP_GRACE);
                tick_tx.send(Event::StartupFinished);
            })
            .expect("failed to spawn startup thread");

        // The config: the watcher feeds reloads to the reactor; the effect runner applies
        // each applied config to what the event taps read.
        let shared = Shared::new(&initial);
        shared.attach_bus(Arc::clone(&bus));
        let watch_state = match &initial_text {
            Some(text) => WatchState::seeded_from_text(source.clone(), text),
            None => WatchState::new(ConfigSource::BuiltIn),
        };
        let (watch_handle, _watcher) = watcher::spawn(watch_state, events_tx.clone(), POLL_INTERVAL);
        let tap_keeper = keeper::keep_taps(Arc::clone(&shared), events_tx.clone());

        // The control socket: user-only, served on its own threads.
        let listener = match ipc::socket::bind(&socket) {
            Ok(listener) => listener,
            Err(error) => {
                error!(path = %socket.display(), %error, "cannot start: the control socket is unavailable");
                std::process::exit(1);
            }
        };
        let backend = Arc::new(ReactorBackend::new(events_tx.clone(), Arc::clone(&shared)));
        if let Err(error) = ipc::spawn(listener, backend, Arc::clone(&bus)) {
            error!(%error, "cannot start: the control socket server did not start");
            std::process::exit(1);
        }
        info!(path = %socket.display(), "listening");

        let (wm_controller, wm_tx) = WmController::new(events_tx.clone());
        let window_notify = WindowNotify::new(events_tx.clone(), notify_rx, WINDOW_EVENTS);
        let notification_center = NotificationCenter::new(mtm, wm_tx, events_tx);

        info!(pid = std::process::id(), "dowm started");
        Executor::run_main(mtm, async move {
            tokio::join!(
                wm_controller.run(),
                notification_center.watch_for_notifications(),
                window_notify.run(),
                runner::run(effects_rx, shared, watch_handle),
                menubar::run(mtm, menu_rx),
                tap_keeper,
            );
        });
    }

    fn init_logging(filter: Option<&str>) {
        let filter = match filter {
            Some(f) => EnvFilter::new(f),
            None => {
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("dowm=info"))
            }
        };
        tracing_subscriber::fmt().with_env_filter(filter).with_thread_names(true).init();
    }

    /// A panic on any actor thread would leave dowm half-alive; stop the process instead.
    fn install_panic_hook() {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_hook(info);
            std::process::abort();
        }));
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::Cli;
        use clap::Parser;

        #[test]
        fn the_config_flag_is_optional_and_takes_a_path() {
            assert_eq!(Cli::try_parse_from(["dowm"]).unwrap().config, None);
            assert_eq!(
                Cli::try_parse_from(["dowm", "--config", "/tmp/x.toml"]).unwrap().config,
                Some(PathBuf::from("/tmp/x.toml"))
            );
            assert!(Cli::try_parse_from(["dowm", "--config"]).is_err());
            assert!(Cli::try_parse_from(["dowm", "--workspaces", "4"]).is_err(), "the pool comes from the config now");
        }

        #[test]
        fn a_subcommand_or_a_bare_command_selects_the_client() {
            assert!(Cli::try_parse_from(["dowm"]).unwrap().command.is_none());
            assert!(matches!(
                Cli::try_parse_from(["dowm", "list-windows"]).unwrap().command,
                Some(dowm::cli::ClientCommand::ListWindows { .. })
            ));
            assert!(matches!(
                Cli::try_parse_from(["dowm", "workspace", "3"]).unwrap().command,
                Some(dowm::cli::ClientCommand::Run(_))
            ));
            let with_socket = Cli::try_parse_from(["dowm", "--socket", "/tmp/s", "mode"]).unwrap();
            assert_eq!(with_socket.socket, Some(PathBuf::from("/tmp/s")));
            let flags = Cli::try_parse_from(["dowm", "move-node-to-workspace", "--focus-follows-window", "2"]).unwrap();
            assert!(matches!(flags.command, Some(dowm::cli::ClientCommand::Run(w)) if w.len() == 3));
        }
    }
}
