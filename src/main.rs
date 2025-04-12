use applications::{
    child::{populate_initial_state_lock, CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER},
    monitor::{
        handle_dead_applications, handle_new_client_applications, handle_new_system_applications,
        monitor_application_resource_usage, update_client_state, update_system_state,
    },
    resolve::{resolve_client_applications, resolve_system_applications},
};
use artisan_middleware::dusa_collection_utils::{
    errors::ErrorArrayItem,
    logger::LogLevel,
    types::{rwarc::LockWithTimeout, stringy::Stringy},
};
use artisan_middleware::{aggregator::AppStatus, state_persistence::AppState};
use artisan_middleware::{dusa_collection_utils::log, identity::Identifier};
use network::process_tcp;
use std::{collections::HashMap, sync::Arc, time::Duration};
use system::{
    control::{GlobalState, GLOBAL_STATE},
    portal::connect_with_portal,
    signals::{handle_signal, reload_callback, shutdown_callback},
};
use tokio::{net::TcpListener, signal::unix::SignalKind, time::sleep};

mod applications;
mod network;
mod system;

pub type AppStatusArray = LockWithTimeout<HashMap<Stringy, AppStatus>>;

#[tokio::main]
async fn main() -> Result<(), ErrorArrayItem> {
    GlobalState::initialize_global_state().await?;
    let global_state: &Arc<GlobalState> = GLOBAL_STATE.get().unwrap();
    let mut app_state: AppState = global_state.get_state_clone().await?;

    // loading configuration and state persistence
    if app_state.config.debug_mode {
        log!(LogLevel::Debug, "\n{}", app_state);
    }

    {
        if let Err(_) = Identifier::load_from_file() {
            log!(LogLevel::Warn, "Creating new machine id");
            let id = Identifier::new().await.unwrap();
            id.save_to_file().unwrap();
        }
    }
    {
        resolve_client_applications(&app_state.config).await?;
        resolve_system_applications().await?;
        populate_initial_state_lock(&mut app_state).await?;
    }

    // seting up signal listeners
    tokio::spawn(async move {
        if let Err(e) = handle_signal(
            SignalKind::hangup(),
            || global_state.signals.signal_reload(),
            "SIGHUP",
        )
        .await
        {
            log!(LogLevel::Error, "Error handling SIGHUP: {}", e);
        }
    });

    tokio::spawn(async move {
        if let Err(e) = handle_signal(
            SignalKind::user_defined1(),
            || global_state.signals.signal_shutdown(),
            "SIGUSR1",
        )
        .await
        {
            log!(LogLevel::Error, "Error handling SIGUSR1: {}", e);
        }
    });

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = global_state.signals.reload_notify.notified() => {
                    reload_callback(&global_state).await;
                }
                _ = global_state.signals.shutdown_notify.notified() => {
                    shutdown_callback(&global_state).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    log!(LogLevel::Info, "CTRL + C received");
                    global_state.signals.signal_shutdown();
                }
            }
        }
    });

    tokio::spawn(async move {
        loop {
            let mut app_state: AppState = match global_state.get_state_clone().await {
                Ok(state) => state,
                Err(err) => {
                    log!(LogLevel::Error, "Failed to get state data skipping periodics: {}", err.err_mesg);
                    continue;
                },
            };

            if let Err(err) = handle_new_system_applications().await {
                log!(LogLevel::Error, "{}", err);
            };
            sleep(Duration::from_millis(150)).await;

            if let Err(err) = handle_new_client_applications(&mut app_state).await {
                log!(LogLevel::Error, "{}", err);
            };
            sleep(Duration::from_millis(150)).await;

            if let Err(err) =
                monitor_application_resource_usage(SYSTEM_APPLICATION_HANDLER.clone()).await
            {
                log!(LogLevel::Error, "{}", err);
            };
            sleep(Duration::from_millis(150)).await;

            if let Err(err) =
                monitor_application_resource_usage(CLIENT_APPLICATION_HANDLER.clone()).await
            {
                log!(LogLevel::Error, "{}", err);
            };
            sleep(Duration::from_millis(150)).await;

            if let Err(err) = handle_dead_applications().await {
                log!(LogLevel::Error, "{}", err);
            }
            sleep(Duration::from_millis(150)).await;

            if let Err(err) = update_client_state(&mut app_state).await {
                log!(LogLevel::Error, "{}", err);
            }
            sleep(Duration::from_millis(150)).await;

            if let Err(err) = update_system_state().await {
                log!(LogLevel::Error, "{}", err);
            }
            sleep(Duration::from_millis(150)).await;
        }
    });

    // Regiser with portal
    tokio::spawn(async move {
        loop {
            let app_state: Result<AppState, ErrorArrayItem> = global_state.get_state_clone().await;

            match app_state {
                Ok(mut state) => {
                    if let Err(err) = connect_with_portal(&mut state).await {
                        log!(LogLevel::Error, "Failed to connect with portal: {}", err);
                    }
                },
                Err(err) => {
                    log!(LogLevel::Error, "Failed to get state: {}", err);
                },
            }

            sleep(Duration::from_secs(30)).await;
        }
    });

    // Initiating network stack
    let tcp_listener: TcpListener = TcpListener::bind(format!("0.0.0.0:9800"))
        .await
        .map_err(|err| ErrorArrayItem::from(err))?;

    loop {
        tokio::select! {
            Ok(conn) = tcp_listener.accept() => {
                tokio::spawn(async move {
                    if let Err(err) = process_tcp(conn).await {
                        log!(LogLevel::Error, "TCP connection handling panicked: {:?}", err);
                    }
                });
            }
        }
    }
}
