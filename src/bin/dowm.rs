//! The dowm daemon.
//!
//! M1: tracks every window and display and logs window/display events live. It does
//! not move windows yet.
//!
//! Threads: the reactor runs on its own thread; each app's AX actor on another (spawned
//! by the WM controller); CGS notifications are translated on a dispatcher thread. The
//! main thread runs AppKit plus the WM controller, NSWorkspace/display notifications
//! and CGS registration.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("dowm only runs on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() { daemon::main() }

#[cfg(target_os = "macos")]
mod daemon {
    use clap::Parser;
    use dowm::actor;
    use dowm::actor::notification_center::NotificationCenter;
    use dowm::actor::reactor::Reactor;
    use dowm::actor::reactor::animation::{AnimationManager, AnimationSender};
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

    #[derive(Parser)]
    #[command(name = "dowm", about = "dowm window manager daemon")]
    struct Cli {
        /// Log filter (tracing `EnvFilter` syntax). Defaults to `RUST_LOG`, else `dowm=info`.
        #[arg(long)]
        log: Option<String>,
        /// How many numbered workspaces to create (until the config file lands in step 5).
        #[arg(long, default_value_t = 9, value_parser = clap::value_parser!(u32).range(1..=64))]
        workspaces: u32,
    }

    pub fn main() {
        let cli = Cli::parse();
        init_logging(cli.log.as_deref());
        install_panic_hook();

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

        let workspaces = dowm::model::workspace::WorkspaceConfig::numbered(cli.workspaces);
        let mut reactor = Reactor::with_workspaces(Some(notify_tx), workspaces)
            .expect("a numbered workspace pool is always valid");
        reactor.set_parked_registry(registry);
        reactor.set_presenter(presenter_tx);
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

        let (wm_controller, wm_tx) = WmController::new(events_tx.clone());
        let window_notify = WindowNotify::new(events_tx.clone(), notify_rx, WINDOW_EVENTS);
        let notification_center = NotificationCenter::new(mtm, wm_tx, events_tx);

        info!(pid = std::process::id(), "dowm started");
        Executor::run_main(mtm, async move {
            tokio::join!(
                wm_controller.run(),
                notification_center.watch_for_notifications(),
                window_notify.run(),
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

        #[test]
        fn the_workspace_count_defaults_to_nine_and_must_be_positive() {
            assert_eq!(Cli::try_parse_from(["dowm"]).unwrap().workspaces, 9);
            assert_eq!(Cli::try_parse_from(["dowm", "--workspaces", "4"]).unwrap().workspaces, 4);
            assert!(Cli::try_parse_from(["dowm", "--workspaces", "0"]).is_err());
            assert!(Cli::try_parse_from(["dowm", "--workspaces", "100"]).is_err());
        }
    }
}
