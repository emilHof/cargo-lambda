use std::sync::Arc;

use crate::{
    error::ServerError,
    requests::{InvokeRequest, NextEvent},
    state::{ExtensionCache, RuntimeState},
    watcher::WatcherConfig,
    CargoOptions,
};
use cargo_lambda_invoke::DEFAULT_PACKAGE_FUNCTION;
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    oneshot,
};
use tokio_graceful_shutdown::SubsystemHandle;
use tracing::{error, info};
use watchexec::{command::Command, Watchexec};

pub(crate) async fn init_scheduler(
    subsys: &SubsystemHandle,
    state: RuntimeState,
    cargo_options: CargoOptions,
    watcher_config: WatcherConfig,
) -> Sender<InvokeRequest> {
    let (req_tx, req_rx) = mpsc::channel::<InvokeRequest>(100);

    subsys.start("lambda scheduler", move |s| async move {
        start_scheduler(s, state, cargo_options, watcher_config, req_rx).await
    });

    req_tx
}

async fn start_scheduler(
    subsys: SubsystemHandle,
    state: RuntimeState,
    cargo_options: CargoOptions,
    watcher_config: WatcherConfig,
    mut req_rx: Receiver<InvokeRequest>,
) -> Result<(), ServerError> {
    let (gc_tx, mut gc_rx) = mpsc::channel::<String>(10);
    let (wx_tx, mut wx_rx) = mpsc::channel::<(
        tokio::sync::oneshot::Sender<Result<Arc<Watchexec>, ServerError>>,
        Command,
        WatcherConfig,
        ExtensionCache,
    )>(10);

    tokio::spawn(async move {
        while let Some((tx, cmd, watcher_config, ext_cache)) = wx_rx.recv().await {
            let wx = crate::watcher::new(cmd, watcher_config, ext_cache).await;
            let _ = tx.send(wx);
        }
    });

    loop {
        tokio::select! {
            Some(req) = req_rx.recv() => {
                let result = state.req_cache.upsert(req).await?;
                if let Some((name, api)) = result {
                    if !watcher_config.only_lambda_apis {
                        let gc_tx = gc_tx.clone();
                        let cargo_options = cargo_options.clone();
                        let watcher_config = watcher_config.clone();
                        let ext_cache = state.ext_cache.clone();
                        let wx_tx = wx_tx.clone();
                        subsys.start("lambda runtime", move |s| start_function(s, name, api, cargo_options, watcher_config, gc_tx, ext_cache, wx_tx));
                    }
                }
            },
            Some(gc) = gc_rx.recv() => {
                state.req_cache.clean(&gc).await;
            },
            _ = subsys.on_shutdown_requested() => {
                info!("terminating lambda scheduler");
                return Ok(());
            },

        };
    }
}

async fn start_function(
    subsys: SubsystemHandle,
    name: String,
    runtime_api: String,
    cargo_options: CargoOptions,
    mut watcher_config: WatcherConfig,
    gc_tx: Sender<String>,
    ext_cache: ExtensionCache,
    wx_tx: Sender<(
        tokio::sync::oneshot::Sender<Result<Arc<Watchexec>, ServerError>>,
        Command,
        WatcherConfig,
        ExtensionCache,
    )>,
) -> Result<(), ServerError> {
    info!(function = ?name, manifest = ?cargo_options.manifest_path, "starting lambda function");

    let cmd = cargo_command(&name, &cargo_options);
    watcher_config.bin_name = if is_valid_bin_name(&name) {
        Some(name.clone())
    } else {
        None
    };
    watcher_config.name = name.clone();
    watcher_config.runtime_api = runtime_api;

    let (tx, rx) = oneshot::channel();

    let _ = wx_tx
        .send((tx, cmd, watcher_config, ext_cache.clone()))
        .await;

    let wx = rx.blocking_recv()??;

    tokio::select! {
        _ = wx.main() => {
            if let Err(err) = gc_tx.send(name.clone()).await {
                error!(error = %err, function = ?name, "failed to send message to cleanup dead function");
            }
        },
        _ = subsys.on_shutdown_requested() => {
            info!(function = ?name, "terminating lambda function");
        }
    }

    let event = NextEvent::shutdown(&format!("{name} function shutting down"));
    ext_cache.send_event(event).await
}

fn is_valid_bin_name(name: &str) -> bool {
    !name.is_empty() && name != DEFAULT_PACKAGE_FUNCTION
}

fn cargo_command(name: &str, cargo_options: &CargoOptions) -> watchexec::command::Command {
    let mut args = vec!["run".into()];
    if let Some(features) = cargo_options.features.as_deref() {
        args.push("--features".into());
        args.push(features.into());
    }

    if cargo_options.release {
        args.push("--release".into());
    }

    if is_valid_bin_name(name) {
        args.push("--bin".into());
        args.push(name.into());
    }

    Command::Exec {
        prog: "cargo".into(),
        args,
    }
}
