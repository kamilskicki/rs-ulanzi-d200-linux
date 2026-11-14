mod config;
mod daemon;
mod device;
mod openaction_client;
mod press_filter;
mod system_monitor;
mod action;

use anyhow::{bail, Result};
use clap::Parser;
use log::{error, info, warn};
use std::path::PathBuf;
use openaction::*;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the configuration YAML file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    /// Log level (info, debug, trace)
    #[arg(short, long, default_value = "info")]
    log_level: String,

    /// WebSocket server port
    #[arg(long, short = 'p', default_value_t = 57116)]
    port: u16,

    /// Stream Deck / OpenDeck Registration: Plugin UUID
    #[arg(long = "pluginUUID", hide = true)]
    plugin_uuid: Option<String>,

    /// Stream Deck / OpenDeck Registration: Register Event
    #[arg(long = "registerEvent", hide = true)]
    register_event: Option<String>,

    /// Stream Deck / OpenDeck Registration: Info
    #[arg(long = "info", hide = true)]
    info: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Preprocess arguments to handle Stream Deck's single-dash parameters
    let args_iter = std::env::args().map(|arg| match arg.as_str() {
        "-port" => "--port".to_string(),
        "-pluginUUID" => "--pluginUUID".to_string(),
        "-registerEvent" => "--registerEvent".to_string(),
        "-info" => "--info".to_string(),
        _ => arg,
    });

    let args = Args::parse_from(args_iter);

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&args.log_level))
        .init();

    info!("Starting Ulanzi D200 driver {}", env!("CARGO_PKG_VERSION"));

    // The driver only runs as an OpenDeck plugin (OpenDeck passes -pluginUUID etc.).
    let Some(uuid) = args.plugin_uuid.clone() else {
        bail!("This driver is started by OpenDeck (missing -pluginUUID); it is not a standalone program");
    };

    // 1. Load configuration; a missing or invalid file falls back to the built-in defaults.
    let config = match config::Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "Configuration file not found or invalid ({}), using defaults: {}",
                args.config.display(),
                e
            );
            config::Config::default()
        }
    };

    // Create the cycle command channel
    let (cycle_tx, cycle_rx) = mpsc::channel(1);

    // 2. Connect to OpenDeck
    let (plugin_cmd_rx, hw_event_tx, openaction_handle) = {
        let port = args.port;
        let event = args
            .register_event
            .clone()
            .unwrap_or_else(|| "register".to_string());
        let info_str = args.info.clone().unwrap_or_else(|| "{}".to_string());

        info!("Connecting to OpenDeck (UUID: {}, Port: {})", uuid, port);

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(100);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(100);

        let bridge = crate::openaction_client::OpenActionBridge::new(cmd_tx);
        bridge.register();

        let oa_args = vec![
            "rs-ulanzi-d200-linux".to_string(),
            "-port".to_string(),
            port.to_string(),
            "-pluginUUID".to_string(),
            uuid.clone(),
            "-registerEvent".to_string(),
            event,
            "-info".to_string(),
            info_str,
        ];

        // Keep handle so we can detect when OpenDeck disconnects
        let oa_handle = tokio::spawn(async move {
            info!("Running OpenAction Runtime...");
            if let Err(e) = openaction::run(oa_args).await {
                error!("OpenAction Runtime Error: {}", e);
            }
            info!("OpenAction Runtime exited");
        });

        // Register the action with the cycle sender
        let cycle_action = action::CycleStatusWindow { cycle_tx: cycle_tx.clone() };
        register_action(cycle_action).await;

        // Outbound events forwarder (unchanged)
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    daemon::HardwareEvent::KeyDown {
                        device_id,
                        key_index,
                    } => {
                        let _ = openaction::device_plugin::key_down(device_id, key_index).await;
                    }
                    daemon::HardwareEvent::KeyUp {
                        device_id,
                        key_index,
                    } => {
                        let _ = openaction::device_plugin::key_up(device_id, key_index).await;
                    }
                    daemon::HardwareEvent::DeviceConnected { device_id } => {
                        let _ = openaction::device_plugin::register_device(
                            device_id,
                            "Ulanzi D200".to_string(),
                            3,
                            5,
                            0,
                            0,
                        )
                        .await;
                    }
                }
            }
        });

        (Some(cmd_rx), Some(event_tx), oa_handle)
    };

    // 3. Run the daemon until it or the OpenAction runtime ends
    let daemon = daemon::UlanziDaemon::new(config, plugin_cmd_rx, hw_event_tx, cycle_rx).await?;

    tokio::select! {
        res = daemon.run() => {
            info!("Daemon exited");
            res?;
        }
        _ = openaction_handle => {
            info!("OpenAction runtime ended; shutting down driver");
        }
    }
    Ok(())
}
