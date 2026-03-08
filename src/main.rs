use applications::{
    child::upsert_local_manager_state,
    watchdog_sync::{refresh_logs_from_state_files, refresh_status_from_watchdog},
};
use artisan_middleware::dusa_collection_utils::core::{
    errors::ErrorArrayItem,
    logger::LogLevel,
    types::{pathtype::PathType, rwarc::LockWithTimeout, stringy::Stringy},
};
use artisan_middleware::{
    aggregator::AppStatus,
    config::AppConfig,
    state_persistence::{AppState, StatePersistence},
};
use artisan_middleware::{dusa_collection_utils::log, identity::Identifier};
use network::process_tcp;
use std::{collections::HashMap, sync::Arc, time::Duration};
use system::{
    config::{generate_state, get_config},
    control::Controls,
    portal::connect_with_portal,
    state::{get_state_path, save_state},
};
use tokio::{net::TcpListener, time::sleep};

mod applications;
mod network;
mod system;
mod watchdog;

pub type AppStatusArray = LockWithTimeout<HashMap<Stringy, AppStatus>>;

#[tokio::main]
async fn main() -> Result<(), ErrorArrayItem> {
    // loading configuration and state persistence
    let config: AppConfig = get_config();
    let mut state: AppState = generate_state(&config).await;
    let state_path: PathType = get_state_path(&config);
    if config.debug_mode {
        log!(LogLevel::Debug, "\n{}", state);
    }

    {
        if let Err(_) = Identifier::load_from_file() {
            log!(LogLevel::Warn, "Creating new machine id");
            let id = Identifier::new().await.unwrap();
            id.save_to_file().unwrap();
        }
    }
    {
        upsert_local_manager_state(&state).await?;
        if let Err(err) = refresh_status_from_watchdog().await {
            log!(LogLevel::Warn, "Initial watchdog inventory sync failed: {}", err);
        }
    }

    // seting up trackers
    match Controls::initialize_controls().await {
        Ok(controls) => Arc::new(controls),
        Err(err) => return Err(err),
    };

    let application_controls: Arc<Controls> = Controls::get_controls().await?;
    

    // setting up controls and signal monitoring
    application_controls.start_signal_monitors();
    application_controls
        .clone()
        .start_contol_monitor();

    // Sync inventory/status/metrics from watchdog.
    tokio::spawn(async move {
        loop {
            if let Err(err) = refresh_status_from_watchdog().await {
                log!(LogLevel::Warn, "Watchdog status sync failed: {}", err);
            }
            sleep(Duration::from_secs(2)).await;
        }
    });

    // Refresh logs from per-app state files (client-app direct stdout/stderr).
    tokio::spawn(async move {
        loop {
            if let Err(err) = refresh_logs_from_state_files().await {
                log!(LogLevel::Warn, "State-file log refresh failed: {}", err);
            }
            sleep(Duration::from_secs(5)).await;
        }
    });

    // Keep the watchdog connection marker fresh for watchdog `manager_linked` detection.
    let state_path_for_watchdog = state_path.clone();
    tokio::spawn(async move {
        let mut was_connected = false;
        loop {
            let connected = crate::watchdog::is_reachable(Duration::from_millis(800)).await;
            let mut sleep_for = Duration::from_secs(if connected { 10 } else { 30 });

            let should_persist = connected || connected != was_connected;
            if should_persist {
                let manager_state = match StatePersistence::load_state(&state_path_for_watchdog).await
                {
                    Ok(state) => Some(state),
                    Err(_) => None,
                };

                if let Some(mut manager_state) = manager_state {
                    manager_state.data = crate::system::state::upsert_watchdog_connection_marker(
                        &manager_state.data,
                        connected.then_some(crate::watchdog::socket_path()),
                    );
                    crate::system::state::save_state(&mut manager_state, &state_path_for_watchdog)
                        .await;
                } else {
                    sleep_for = Duration::from_secs(1);
                }
            }

            was_connected = connected;
            sleep(sleep_for).await;
        }
    });

    // Regiser with portal
    let mut state_clone = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = connect_with_portal(&mut state_clone).await {
                log!(LogLevel::Error, "{}", err)
            }

            sleep(Duration::from_secs(30)).await;
        }
    });

    // Initiating network stack
    let tcp_listener: TcpListener = TcpListener::bind(format!("0.0.0.0:9800"))
        .await
        .map_err(|err| ErrorArrayItem::from(err))?;

    let state_path_clone = state_path.clone();
    let config_clone = config.clone();
    loop {
        tokio::select! {
            Ok(conn) = tcp_listener.accept() => {
                let app_controls = application_controls.clone();
                let mut state_clone = state.clone();
                let state_path_clone = state_path_clone.clone();
                let config_clone = config_clone.clone();

                state.event_counter += 1;
                save_state(&mut state, &state_path_clone).await;
                tokio::spawn(async move {
                    if let Err(err) = process_tcp(conn, app_controls, &mut state_clone, &state_path_clone, &config_clone).await {
                        log!(LogLevel::Error, "TCP connection handling panicked: {:?}", err);
                    }
                });
            }
        }
    }
}
