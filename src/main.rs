// Release builds on Windows use the GUI subsystem, so launching Lan Mouse
// (e.g. from the tray autostart) doesn't open a console window that kills
// the app when closed. See `init_logging` for where logs go instead.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use lan_mouse_cli::CliError;
#[cfg(feature = "gtk")]
use lan_mouse_gtk::GtkError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
use std::{
    future::Future,
    io,
    process::{self, Child},
};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum LanMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    init_logging();

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn init_logging() {
    let env = Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info");
    let mut builder = env_logger::Builder::from_env(env);
    #[cfg(all(windows, not(debug_assertions)))]
    windows_log_target(&mut builder);
    builder.init();
}

/// A GUI-subsystem process has no console. When started from a terminal,
/// attach to it so CLI output and logs still show up there; otherwise
/// (Start menu, autostart, the daemon child) log to a file in the config
/// directory, e.g. `%LOCALAPPDATA%\lan-mouse\lan-mouse.log`.
#[cfg(all(windows, not(debug_assertions)))]
fn windows_log_target(builder: &mut env_logger::Builder) {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok() {
        return;
    }
    let Some(dir) =
        std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("lan-mouse"))
    else {
        return;
    };
    // The GUI and its daemon child run at the same time: give each its own file.
    let name = if std::env::args().any(|a| a == "daemon") {
        "lan-mouse-daemon.log"
    } else {
        "lan-mouse.log"
    };
    let file = std::fs::create_dir_all(&dir).and_then(|_| std::fs::File::create(dir.join(name)));
    if let Ok(file) = file {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(feature = "gtk")]
            {
                // Only spawn a new daemon if one isn't already running
                let mut service = if lan_mouse_ipc::is_service_running() {
                    log::info!("daemon already running, connecting to existing instance");
                    None
                } else {
                    Some(start_service()?)
                };
                let res = lan_mouse_gtk::run(config::local_commit(), config.hidden());
                if let Some(ref mut service) = service {
                    #[cfg(unix)]
                    {
                        // on unix we give the service a chance to terminate gracefully
                        let pid = service.id() as libc::pid_t;
                        unsafe {
                            libc::kill(pid, libc::SIGINT);
                        }
                        service.wait()?;
                    }
                    service.kill()?;
                }
                res?;
            }
            #[cfg(not(feature = "gtk"))]
            {
                // run daemon if gtk is diabled
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

fn start_service() -> Result<Child, io::Error> {
    let child = process::Command::new(std::env::current_exe()?)
        .args(std::env::args().skip(1))
        .arg("daemon")
        .spawn()?;
    Ok(child)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
