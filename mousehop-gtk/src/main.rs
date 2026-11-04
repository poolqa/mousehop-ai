#![cfg_attr(windows, windows_subsystem = "windows")]
shadow_rs::shadow!(build);
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("mousehop-ui {}", build::PKG_VERSION);
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--check-template") {
        if let Err(error) = mousehop_gtk::validate_settings_template() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    // Busy or denied IPC is not a missing Agent. Relaunching on these errors
    // feeds the Agent present callback back into another Settings launch.
    if std::env::args().nth(1).as_deref() != Some("transfer")
        && mousehop_ipc::connect_once()
            .err()
            .is_some_and(|error| error.is_endpoint_missing())
    {
        if let Ok(exe) = std::env::current_exe() {
            let runtime = exe.with_file_name(format!("mousehop{}", std::env::consts::EXE_SUFFIX));
            if let Err(error) = std::process::Command::new(runtime).arg("agent").spawn() {
                eprintln!("Start Mousehop: {error}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let role = if std::env::args().nth(1).as_deref() == Some("transfer") {
            "transfer"
        } else {
            "gui"
        };
        if let Err(error) = mousehop_logging::user_log_directory().and_then(|directory| {
            mousehop_logging::init(role, directory.clone())?;
            mousehop_logging::install_panic(role, directory);
            Ok(())
        }) {
            eprintln!("Cannot initialize Mousehop GUI file logging: {error}");
            let _ = env_logger::Builder::from_env(
                env_logger::Env::default().filter_or("MOUSEHOP_LOG_LEVEL", "info"),
            )
            .try_init();
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    env_logger::Builder::from_env(
        env_logger::Env::default().filter_or("MOUSEHOP_LOG_LEVEL", "info"),
    )
    .init();
    let mut commit = [b'?'; 8];
    let bytes = build::SHORT_COMMIT.as_bytes();
    let n = bytes.len().min(8);
    commit[..n].copy_from_slice(&bytes[..n]);
    let info = mousehop_gtk::BuildInfo {
        local_commit: commit,
        version: build::PKG_VERSION,
        build_time: build::BUILD_TIME,
        rust_version: build::RUST_VERSION,
        source_url: "https://github.com/jondkinney/mousehop",
    };
    let result = if let Some(id) = std::env::args()
        .nth(1)
        .filter(|a| a == "transfer")
        .and_then(|_| std::env::args().nth(2))
    {
        mousehop_gtk::run_transfer(info, &id)
    } else {
        mousehop_gtk::run(info)
    };
    if let Err(error) = result {
        log::error!("{error}");
        std::process::exit(1);
    }
}
