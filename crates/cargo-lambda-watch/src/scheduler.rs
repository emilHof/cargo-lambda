use crate::{
    requests::Action,
    state::RuntimeState,
    watcher::{FunctionData, WatcherConfig},
    CargoOptions, ServerError,
};
use cargo_lambda_invoke::DEFAULT_PACKAGE_FUNCTION;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_graceful_shutdown::SubsystemHandle;
use tracing::{error, info};
use watchexec::command::Program;
use watchexec_events::{Event, Priority};

pub(crate) async fn init_scheduler(
    subsys: &SubsystemHandle,
    state: RuntimeState,
    cargo_options: CargoOptions,
    watcher_config: WatcherConfig,
) -> Sender<Action> {
    let (req_tx, req_rx) = mpsc::channel::<Action>(100);

    subsys.start("lambda scheduler", move |s| async move {
        start_scheduler(s, state, cargo_options, watcher_config, req_rx).await;
        Ok::<_, std::convert::Infallible>(())
    });

    req_tx
}

async fn start_scheduler(
    subsys: SubsystemHandle,
    state: RuntimeState,
    cargo_options: CargoOptions,
    watcher_config: WatcherConfig,
    mut req_rx: Receiver<Action>,
) {
    let (function_tx, function_rx) = mpsc::channel::<FunctionData>(10);
    let (gc_tx, mut gc_rx) = mpsc::channel::<String>(10);
    let function_rx = Arc::new(Mutex::new(function_rx));

    let wx = crate::watcher::new(
        watcher_config.clone(),
        state.ext_cache.clone(),
        function_rx,
        gc_tx,
    )
    .await
    .expect("watcher to start");

    // Start watcher process and main scheduler loop.
    tokio::select! {
        res = wx.main() => match res {
            Ok(_) => {},
            Err(error) => {
                error!(?error, "failed to obtain watchexec task");
                subsys.request_global_shutdown();
            }
        },
        _ = {
            let wx = wx.clone();
            let subsys = subsys.clone();
            async move {
                loop {
                    tokio::select! {
                        Some(action) = req_rx.recv() => {
                            let start_function_name = match action {
                                Action::Invoke(req) => {
                                    match state.req_cache.upsert(req).await {
                                        Err(_) => None,
                                        Ok(v) => v,
                                    }
                                },
                                Action::Init => {
                                    state.req_cache.init(DEFAULT_PACKAGE_FUNCTION).await;
                                    Some(DEFAULT_PACKAGE_FUNCTION.into())
                                }
                            };

                            if watcher_config.start_function() {
                                if let Some(name) = start_function_name {
                                    let runtime_api = format!("{}/{}", &state.server_addr, &name);
                                    info!(function = name, "starting new lambda");
                                    let function_data = match function_data(
                                        name,
                                        runtime_api,
                                        cargo_options.clone()
                                    ) {
                                        Err(err) => {
                                            error!(error = ?err, "failed to initialize function with data");
                                            continue;
                                        }
                                        Ok(data) => data
                                    };

                                    // Check for errors sending function or event.
                                    if let Err(err) =
                                        function_tx.send(function_data.clone()).await
                                    {
                                        error!(error = ?err, "failed to send function data");
                                    }
                                    if let Err(err) =
                                        wx.send_event(Event::default(), Priority::High).await
                                    {
                                        error!(error = ?err, "failed to send event");
                                    }
                                }
                            }
                        },
                        Some(name) = gc_rx.recv() => {
                            state.req_cache.clean(&name).await;
                        },
                        _ = subsys.on_shutdown_requested() => {
                            info!("terminating lambda scheduler");
                            return;
                        },
                    };
                }
            }
        } => {}
    };
}

fn function_data(
    name: String,
    runtime_api: String,
    cargo_options: CargoOptions,
) -> Result<FunctionData, ServerError> {
    let cmd = cargo_command(&name, &cargo_options)?;
    let bin_name = if is_valid_bin_name(&name) {
        Some(name.clone())
    } else {
        None
    };

    Ok(FunctionData {
        cmd,
        name,
        runtime_api,
        bin_name,
    })
}

fn is_valid_bin_name(name: &str) -> bool {
    !name.is_empty() && name != DEFAULT_PACKAGE_FUNCTION
}

pub(crate) fn cargo_command(
    name: &str,
    cargo_options: &CargoOptions,
) -> Result<watchexec::command::Command, ServerError> {
    let mp = cargo_options
        .manifest_path
        .to_str()
        .ok_or_else(|| ServerError::InvalidManifest(cargo_options.manifest_path.clone()))?;
    let mut args = vec![
        "run".into(),
        "--manifest-path".into(),
        mp.to_string(),
        "--color".into(),
        cargo_options.color.clone(),
    ];
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

    Ok(watchexec::command::Command::from(Program::Exec {
        prog: "cargo".into(),
        args,
    }))
}
