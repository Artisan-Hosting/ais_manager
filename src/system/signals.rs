use std::sync::Arc;
use std::time::Duration;

use artisan_middleware::aggregator::{save_registered_apps, AppStatus};
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::logger::LogLevel;
use tokio::signal::unix::SignalKind;

use crate::applications::child::{
    APP_STATUS_ARRAY, CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER,
};
use crate::applications::resolve::{resolve_client_applications, resolve_system_applications};

use super::control::GlobalState;

pub async fn handle_signal<F>(
    signal_kind: SignalKind,
    callback: F,
    signal_name: &str,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: Fn() + Send + Sync + 'static,
{
    let mut signal = tokio::signal::unix::signal(signal_kind)?;
    while signal.recv().await.is_some() {
        log!(LogLevel::Info, "Received {}, signaling...", signal_name);
        callback();
    }
    Ok(())
}

pub async fn reload_callback(gs: &Arc<GlobalState>) {
    log!(LogLevel::Info, "Reloading");
    gs.locks.pause_network().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Clearing handlers
    let client_handler = &CLIENT_APPLICATION_HANDLER.clone();
    let system_handler = &SYSTEM_APPLICATION_HANDLER.clone();

    if let Err(err) = client_handler.try_write().await {
        log!(
            LogLevel::Error,
            "Failed to lock client handler, dumping: {}",
            err
        );
    }
    if let Err(err) = system_handler.try_write().await {
        log!(
            LogLevel::Error,
            "Failed to lock system handler, dumping: {}",
            err
        );
    }

    if let Err(err) = resolve_client_applications(&gs.clone()).await {
        log!(LogLevel::Error, "{}", err);
    }

    if let Err(err) = resolve_system_applications().await {
        log!(LogLevel::Error, "{}", err);
    }

    log!(LogLevel::Info, "Reloaded!");
    gs.locks.resume_network().await;
}

pub async fn shutdown_callback(gs: &Arc<GlobalState>) {
    log!(LogLevel::Info, "Shutting down gracefully");
    tokio::time::sleep(Duration::from_millis(200)).await;

    gs.locks.pause_network().await;

    // Clearing the handlers
    let client_handler = &CLIENT_APPLICATION_HANDLER.clone();
    let system_handler = &SYSTEM_APPLICATION_HANDLER.clone();

    let app_status_array = &APP_STATUS_ARRAY.clone();

    if let Err(err) = client_handler.try_write().await {
        log!(
            LogLevel::Error,
            "Failed to lock client handler, dumping: {}",
            err
        );
    }

    if let Err(err) = system_handler.try_write().await {
        log!(
            LogLevel::Error,
            "Failed to lock system handler, dumping: {}",
            err
        );
    }

    // let app_status_array_read_lock = gs.get_state_clone().await.app_status_array();
    let mut app_array: Vec<AppStatus> = Vec::new();

    if let Ok(array_read) = app_status_array.try_read().await {
        _ = array_read
            .clone()
            .into_iter()
            .map(|app| app_array.push(app.1));
    }

    for app in app_array.clone() {
        log!(LogLevel::Debug, "Status: {}", app);
    }

    if let Err(err) = save_registered_apps(&app_array).await {
        log!(LogLevel::Error, "{}", err);
        std::process::exit(1)
    }

    log!(LogLevel::Info, "Bye~");
    std::process::exit(0)
}
