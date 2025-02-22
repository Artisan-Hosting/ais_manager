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
    types::{pathtype::PathType, rwarc::LockWithTimeout, stringy::Stringy},
};
use artisan_middleware::{aggregator::AppStatus, config::AppConfig, state_persistence::AppState};
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
        resolve_client_applications(&state.config).await?;
        resolve_system_applications().await?;
        populate_initial_state_lock(&mut state).await?;
    }

    // seting up trackers
    let application_controls: Arc<Controls> = Arc::new(Controls::new());

    // setting up controls and signal monitoring
    application_controls.start_signal_monitors();
    application_controls
        .clone()
        .start_contol_monitor(state.clone());

    // Update metrics
    let mut state_clone = state.clone();
    // let config_clone = config.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = handle_new_system_applications().await {
                log!(LogLevel::Error, "{}", err);
            };
            sleep(Duration::from_millis(150)).await;

            if let Err(err) = handle_new_client_applications(&mut state_clone).await {
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

            if let Err(err) = update_client_state(&mut state_clone).await {
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
