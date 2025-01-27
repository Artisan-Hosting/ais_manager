use applications::{
    child::{
        populate_initial_state_lock, spawn_client_applications, spawn_system_applications,
        CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER,
    },
    monitor::{monitor_applications, update_application_usage},
    resolve::{
        resolve_client_applications, resolve_system_applications, Applications, ClientApplication,
        SystemApplication,
    },
};
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::{
    errors::ErrorArrayItem, log::LogLevel, rwarc::LockWithTimeout, stringy::Stringy,
    types::PathType,
};
use artisan_middleware::{aggregator::AppStatus, config::AppConfig, state_persistence::AppState};
use network::process_tcp;
use std::{collections::HashMap, sync::Arc, time::Duration};
use system::{
    config::{generate_state, get_config}, control::{Controls, PortalState, PORTAL_CONTROLS}, portal::connect_with_portal, state::{get_state_path, save_state}
};
use tokio::{
    net::TcpListener,
    time::sleep,
};

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

    // seting up trackers
    let application_controls: Arc<Controls> = Arc::new(Controls::new());

    // setting up controls and signal monitoring
    application_controls.start_signal_monitors();
    application_controls.clone().start_contol_monitor();



    // std::process::exit(0);

    // Initiating array for applications.

    // Initiating system applications.
    // Initiating client appications.
    let system_applications: Vec<SystemApplication> = resolve_system_applications().await;
    let client_applications: Vec<ClientApplication> = resolve_client_applications(&config).await;
    // print!("{:#?}", system_applications);
    // print!("{:#?}", client_applications);

    // Spawning applications
    if config.environment != "systemonly" {
        spawn_client_applications(
            CLIENT_APPLICATION_HANDLER.clone(),
            client_applications.clone(),
            &mut state,
            &state_path,
        )
        .await?;
    }

    spawn_system_applications(
        SYSTEM_APPLICATION_HANDLER.clone(),
        system_applications.clone(),
        &mut state,
        &state_path,
    )
    .await?;

    // Adding applications to the status array
    populate_initial_state_lock(Applications::System(system_applications.clone())).await?;

    if config.environment != "systemonly" {
        populate_initial_state_lock(Applications::Client(client_applications.clone())).await?;
    }

    // Update metrics
    tokio::spawn(async move {
        loop {
            if let Err(err) = monitor_applications(SYSTEM_APPLICATION_HANDLER.clone()).await {
                log!(LogLevel::Error, "{}", err);
            };
            if let Err(err) = monitor_applications(CLIENT_APPLICATION_HANDLER.clone()).await {
                log!(LogLevel::Error, "{}", err);
            };
            if let Err(err) = update_application_usage().await {
                log!(LogLevel::Error, "{}", err);
            }
            sleep(Duration::from_secs(2)).await;
        }
    });

    // Regiser with portal 
    let config_clone = config.clone();
    tokio::spawn(async move {
        while !PortalState::is_portal_linked(PORTAL_CONTROLS.clone()).await.unwrap() {
            log!(LogLevel::Debug, "Attempting to register with portals");

            if let Err(err) = connect_with_portal(&config_clone).await {
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

    // Registering manager with portal

    // application_controls.reload_notify.notify_one();
    // println!("reloaded");
    // sleep(Duration::from_secs(2)).await;
    // sleep(Duration::from_secs(30)).await;
    // application_controls.signal_shutdown();

    // Ok(())
}
