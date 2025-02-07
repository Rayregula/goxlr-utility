mod audio;
mod cli;
mod communication;
mod device;
mod files;
mod http_server;
mod mic_profile;
mod primary_worker;
mod profile;
mod settings;
mod shutdown;

use crate::cli::{Cli, LevelFilter};
use crate::files::FileManager;
use crate::http_server::launch_httpd;
use crate::primary_worker::handle_changes;
use crate::settings::SettingsHandle;
use crate::shutdown::Shutdown;
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use communication::listen_for_connections;
use goxlr_ipc::Socket;
use goxlr_ipc::{DaemonRequest, DaemonResponse};
use log::{info, warn};
use simplelog::{ColorChoice, CombinedLogger, Config, TermLogger, TerminalMode};
use std::fs;
use std::fs::remove_file;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::{join, signal};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Cli = Cli::parse();

    CombinedLogger::init(vec![TermLogger::new(
        match args.log_level {
            LevelFilter::Off => log::LevelFilter::Off,
            LevelFilter::Error => log::LevelFilter::Error,
            LevelFilter::Warn => log::LevelFilter::Warn,
            LevelFilter::Info => log::LevelFilter::Info,
            LevelFilter::Debug => log::LevelFilter::Debug,
            LevelFilter::Trace => log::LevelFilter::Trace,
        },
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )])
    .context("Could not configure the logger")?;

    let settings = SettingsHandle::load(args.config).await?;
    let listener = create_listener("/tmp/goxlr.socket").await?;

    let mut perms = fs::metadata("/tmp/goxlr.socket")?.permissions();
    perms.set_mode(0o777);
    fs::set_permissions("/tmp/goxlr.socket", perms)?;

    let mut shutdown = Shutdown::new();
    let file_manager = FileManager::new();
    let (usb_tx, usb_rx) = mpsc::channel(32);
    let usb_handle = tokio::spawn(handle_changes(
        usb_rx,
        shutdown.clone(),
        settings,
        file_manager,
    ));
    let communications_handle = tokio::spawn(listen_for_connections(
        listener,
        usb_tx.clone(),
        shutdown.clone(),
    ));

    let (httpd_tx, httpd_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(launch_httpd(usb_tx.clone(), httpd_tx));
    let http_server = httpd_rx.await?;

    await_ctrl_c(shutdown.clone()).await;

    info!("Shutting down daemon");
    let _ = join!(usb_handle, communications_handle, http_server.stop(true));

    info!("Removing Socket");
    remove_file("/tmp/goxlr.socket")?;
    shutdown.recv().await;
    Ok(())
}

async fn await_ctrl_c(shutdown: Shutdown) {
    if signal::ctrl_c().await.is_ok() {
        shutdown.trigger();
    }
}

async fn create_listener<P: AsRef<Path>>(path: P) -> Result<UnixListener> {
    let path = path.as_ref();
    let mut error = anyhow!("Could not create Unix socket listener");

    for _ in 0..3 {
        if path.exists() {
            if is_already_running(path).await {
                return Err(anyhow!("A GoXLR daemon is already running"));
            } else {
                warn!("Removing unused socket file {}", path.to_string_lossy());
                let _ = remove_file(path);
            }
        }
        match UnixListener::bind(path) {
            Ok(listener) => return Ok(listener),
            Err(e) => {
                error = anyhow::Error::from(e).context("Could not bind the Unix socket");
            }
        }
    }

    Err(error)
}

async fn is_already_running(path: &Path) -> bool {
    let stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(_) => return false,
    };
    let address = match stream.peer_addr() {
        Ok(address) => address,
        Err(_) => return false,
    };
    let mut socket: Socket<DaemonResponse, DaemonRequest> = Socket::new(address, stream);

    if socket.send(DaemonRequest::Ping).await.is_err() {
        return false;
    }

    socket.read().await.is_some()
}
