#![cfg_attr(windows, windows_subsystem = "windows")]
mod agent;
mod autostart;
#[cfg(all(unix, not(target_os = "macos")))]
mod desktop_install;
mod heartbeat;
#[cfg(windows)]
mod i18n;
#[cfg(target_os = "linux")]
mod linux_tray;
#[cfg(target_os = "macos")]
mod macos_tray;
mod paste_control;
mod signals;
mod single_instance;
#[cfg(windows)]
mod windows_tray;
use clap::Parser;
use env_logger::Env;
use mousehop_core::{
    capture_test,
    config::{Command, Config},
    emulation_test,
    service::{Service, ServiceError},
};
use mousehop_ipc::IpcListenerCreationError;
use std::{future::Future, process};
type AppResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
fn main() {
    #[cfg(target_os = "macos")]
    if std::env::args().len() == 2
        && std::env::args().nth(1).as_deref() == Some("permissions-probe")
    {
        process::exit(mousehop_core::macos_tcc_probe::permission_probe_status());
    }
    // Permission probes run repeatedly and must not create a new log file or
    // maintenance thread on every short-lived invocation.
    #[cfg(target_os = "macos")]
    if std::env::args().len() == 2 && std::env::args().nth(1).as_deref() == Some("ax-probe") {
        process::exit(
            if mousehop_core::macos_tcc_probe::is_accessibility_granted() {
                0
            } else {
                1
            },
        );
    }
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("admin-start") {
        let result = (|| -> AppResult {
            if std::env::args().len() != 2
                || !mousehop_ipc::windows::service_mode()
                || !mousehop_ipc::windows::is_elevated()?
            {
                return Err("Elevated installed runtime required".into());
            }
            mousehop_core::windows_service::ensure_running()?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("{error}");
            process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("admin-request") {
        if let Err(error) = mousehop_ipc::windows::execute_admin_request() {
            eprintln!("{error}");
            process::exit(1);
        }
        return;
    }
    // The user reader inherits a private stdout pipe. Route it before console
    // attachment, logging, config loading or any GUI/service initialization.
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("windows-clipboard-read") {
        if std::env::args().len() != 2 {
            process::exit(2);
        }
        if let Err(error) = input_capture::clipboard::write_windows_clipboard_snapshot() {
            eprintln!("{error}");
            process::exit(1);
        }
        return;
    }
    // Windows only, and before anything can log or print: a
    // GUI-subsystem process starts with no standard handles, so hand
    // the console back to invocations that have something to say.
    #[cfg(windows)]
    mousehop_core::windows_console::attach_parent();

    // All desktop processes log to files by default, using the same retention.
    #[cfg(windows)]
    let system_role = std::env::args().find(|arg| {
        matches!(
            arg.as_str(),
            "windows-service" | "windows-session" | "windows-worker"
        )
    });
    #[cfg(windows)]
    if system_role.is_some() {
        if let Err(error) = mousehop_ipc::windows::require_system() {
            eprintln!("{error}");
            process::exit(1);
        }
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let role = if std::env::args().any(|arg| arg == "daemon") {
            "core"
        } else {
            "agent"
        };
        #[cfg(windows)]
        let role = match system_role.as_deref() {
            Some("windows-service") => "service",
            Some("windows-session") => "session",
            Some("windows-worker") => "worker",
            _ => role,
        };
        #[cfg(windows)]
        let directory = if system_role.is_some() {
            mousehop_core::windows_service::data_dir()
        } else {
            mousehop_logging::user_log_directory()
        };
        #[cfg(target_os = "macos")]
        let directory = mousehop_logging::user_log_directory();
        if let Err(error) = directory.and_then(|path| mousehop_logging::init(role, path)) {
            eprintln!("Cannot initialize Mousehop file logging: {error}");
            let _ = env_logger::Builder::from_env(
                Env::default().filter_or("MOUSEHOP_LOG_LEVEL", "info"),
            )
            .try_init();
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    env_logger::Builder::from_env(Env::default().filter_or("MOUSEHOP_LOG_LEVEL", "info")).init();
    #[cfg(windows)]
    mousehop_core::panic_log::install(if mousehop_core::windows_service::worker_mode() {
        "windows-worker"
    } else if std::env::args().any(|a| a == "windows-service" || a == "windows-session") {
        "windows-service"
    } else {
        "agent"
    });
    #[cfg(windows)]
    {
        let result = match std::env::args().nth(1).as_deref() {
            Some("windows-service") => Some(mousehop_core::windows_service::dispatch()),
            _ => None,
        };
        if let Some(result) = result {
            if let Err(e) = result {
                log::error!("{e}");
                process::exit(1);
            }
            return;
        }
    }

    // Route panics to a durable logfile before anything else can run.
    // The daemon child's stderr is /dev/null under LaunchServices, so
    // without this a panic (which `panic = "abort"` turns into an
    // immediate process death) leaves no trace anywhere the user can
    // see — which is exactly why the screensaver-lockup crash went
    // undiagnosed. Tag each record by role so a shared log file stays
    // attributable.
    #[cfg(not(windows))]
    let role = if std::env::args().skip(1).any(|a| a == "daemon") {
        "daemon"
    } else {
        "agent"
    };
    #[cfg(not(windows))]
    mousehop_core::panic_log::install(role);

    // On a Linux `cargo install` (no AUR / Flatpak / distro package)
    // the binary alone wouldn't appear in launchers. This silently
    // writes the .desktop entry + icon into ~/.local/share on the
    // first launch, once. Best-effort — never blocks startup.
    #[cfg(all(unix, not(target_os = "macos")))]
    desktop_install::ensure_first_launch();

    if let Err(e) = run() {
        log::error!("{e}");
        #[cfg(windows)]
        if mousehop_ipc::windows::service_mode() && std::env::args().len() == 1 {
            mousehop_core::windows_service::show_startup_error(&e.to_string());
        }
        process::exit(1);
    }
}

fn run() -> AppResult {
    let config = Config::new()?;
    match config.command() {
        Some(Command::TestEmulation(args)) => run_async(emulation_test::run(config, args))?,
        Some(Command::TestCapture(args)) => run_async(capture_test::run(config, args))?,
        Some(Command::Cli { args }) => {
            let args = mousehop_cli::CliArgs::parse_from(
                std::iter::once("mousehop-cli".to_owned()).chain(args),
            );
            run_async(mousehop_cli::run(args))?;
        }
        #[cfg(windows)]
        Some(Command::WindowsWorker) => {
            mousehop_ipc::windows::require_system()?;
            run_async(mousehop_core::windows_service::desktop_worker(config))?;
        }
        #[cfg(windows)]
        Some(Command::WindowsSession) => {
            run_async(mousehop_core::windows_service::run_session(config))?
        }
        Some(Command::Daemon) => match run_async(run_service(config)) {
            Err(error)
                if error.downcast_ref::<ServiceError>().is_some_and(|e| {
                    matches!(
                        e,
                        ServiceError::IpcListen(IpcListenerCreationError::AlreadyRunning)
                    )
                }) =>
            {
                log::info!("service already running")
            }
            result => result?,
        },
        Some(Command::Firewall(args)) => process::exit(mousehop_core::firewall::run(
            config.port(),
            args.remove,
            args.dry_run,
        )),
        #[cfg(target_os = "macos")]
        Some(Command::AxProbe) => process::exit(
            if mousehop_core::macos_tcc_probe::is_accessibility_granted() {
                0
            } else {
                1
            },
        ),
        Some(Command::Agent) => {
            let first_run =
                config.clients().is_empty() && config.authorized_fingerprints().is_empty();
            drop(config);
            agent::run(first_run)?;
        }
        Some(Command::Settings) => {
            drop(config);
            agent::run(true)?;
        }
        None => {
            let first_run =
                config.clients().is_empty() && config.authorized_fingerprints().is_empty();
            drop(config);
            agent::run(first_run)?;
        }
    }
    Ok(())
}
fn run_async<F, E>(future: F) -> AppResult
where
    F: Future<Output = Result<(), E>>,
    E: std::error::Error + 'static,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(tokio::task::LocalSet::new().run_until(future))?;
    Ok(())
}
async fn run_service(config: Config) -> Result<(), ServiceError> {
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    service.run().await
}
