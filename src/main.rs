use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use artisan_middleware::{
    aggregator::{
        load_registered_apps, save_registered_apps, AppStatus, DeregisterApp, RegisterApp, Status,
    }, config::AppConfig, control::ToggleControl, identity::Identifier, state_persistence::{AppState, StatePersistence}, systemd::SystemdService, timestamp::current_timestamp
};

use config::{generate_initial_state, get_config, get_state_path};
use dusa_collection_utils::{
    errors::ErrorArrayItem,
    functions::del_file,
    log::LogLevel,
    rwarc::{self, LockWithTimeout},
    stringy::Stringy,
    types::PathType,
};
use dusa_collection_utils::{errors::Errors, log};
use external::process_tcp;
use internal::process_unix;
use portal::{get_portal_addr, query_portal, register_with_portal};
use signals::{reload_monitor, shutdown_monitor};
use status::{process_app_status, trim, update_uptime};
use tokio::{
    net::{TcpListener, UnixListener},
    sync::Notify,
    time::{sleep, timeout},
};
mod config;
mod external;
mod internal;
mod portal;
mod signals;
mod status;

type AppStatusStore = rwarc::LockWithTimeout<HashMap<Stringy, AppStatus>>;
type AppUpdateTimeStore = rwarc::LockWithTimeout<HashMap<Stringy, u64>>;
type AppSystemdStore = rwarc::LockWithTimeout<HashMap<Stringy, bool>>;

pub const UPDATE_THRESHOLD: u64 = 30;

#[tokio::main]
async fn main() -> Result<(), ErrorArrayItem> {
    // Defining Initial Application state
    let config: AppConfig = get_config();
    let aggregator: &PathType = &PathType::Content(config.aggregator.clone().unwrap().socket_path);
    let mut state: AppState = generate_initial_state(&config).await;
    let state_path: PathType = get_state_path(&config);

    // Defining signal listening
    let reload_flag: Arc<Notify> = Arc::new(Notify::new());
    let shutdown_flag: Arc<Notify> = Arc::new(Notify::new());
    let execution: Arc<ToggleControl> = Arc::new(ToggleControl::new());
    let portal_setup: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let portal_found: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let reload_flag_clone = reload_flag.clone();
    reload_monitor(reload_flag_clone);

    let shutdown_flag_clone = shutdown_flag.clone();
    shutdown_monitor(shutdown_flag_clone);

    // Debug Information
    if state.config.debug_mode {
        log!(LogLevel::Debug, "loaded config: {}", state.config);
    }

    // Rwlocks for Application status and state pulled from files
    // The aggregator will check for applications that were initialized at the state_persistence level
    // but may not currently be running. This will allow for the aggregator to stop or crash and be restarted
    // and still have awareness of what happened to applications while it wasn't running. IE and application
    // Crashed while the aggregator wasnt running, when the aggregator restarts it can check the state and
    // be expecting an update or notify that it hasn't got an update.
    let app_status_store: LockWithTimeout<HashMap<Stringy, AppStatus>> =
        AppStatusStore::new(HashMap::new());
    let app_update_time_store: LockWithTimeout<HashMap<Stringy, u64>> =
        AppUpdateTimeStore::new(HashMap::new());
    let app_systemd_store: LockWithTimeout<HashMap<Stringy, bool>> =
        AppSystemdStore::new(HashMap::new());

    match load_registered_apps().await {
        Ok(apps) => {
            let mut app_status_store_write_lock: tokio::sync::RwLockWriteGuard<
                '_,
                HashMap<Stringy, AppStatus>,
            > = app_status_store.try_write().await?;

            let mut app_update_time_store_write_lock: tokio::sync::RwLockWriteGuard<
                '_,
                HashMap<Stringy, u64>,
            > = app_update_time_store.try_write().await?;

            for app in apps {
                let app_id: &Stringy = &app.app_id;
                app_update_time_store_write_lock.insert(app_id.to_owned(), current_timestamp());
                app_status_store_write_lock.insert(app_id.to_owned(), app);
            }

            // registering this application
            let self_status: (Stringy, AppStatus) = register_self(&config);
            let self_id: &Stringy = &self_status.0.to_owned();

            app_status_store_write_lock.insert(self_id.to_owned(), self_status.1);
            app_update_time_store_write_lock.insert(self_id.to_owned(), current_timestamp());

            drop(app_status_store_write_lock);
            drop(app_update_time_store_write_lock);
        }
        Err(err) => {
            log!(LogLevel::Warn, "failed to load aggragator state");
            log!(LogLevel::Trace, "{}", err);
            let mut app_status_store_write_lock: tokio::sync::RwLockWriteGuard<
                '_,
                HashMap<Stringy, AppStatus>,
            > = app_status_store.try_write().await?;

            let mut app_update_time_store_write_lock: tokio::sync::RwLockWriteGuard<
                '_,
                HashMap<Stringy, u64>,
            > = app_update_time_store.try_write().await?;

            // registering this application
            let self_status: (Stringy, AppStatus) = register_self(&config);
            let self_id: &Stringy = &self_status.0.to_owned();

            app_status_store_write_lock.insert(self_id.to_owned(), self_status.1);
            app_update_time_store_write_lock.insert(self_id.to_owned(), current_timestamp());

            drop(app_status_store_write_lock);
            drop(app_update_time_store_write_lock);
        }
    }
    state.event_counter += 1;
    update_state(&mut state, &state_path).await;

    // Starting tcp listener
    let tcp_listener: TcpListener = TcpListener::bind(format!("0.0.0.0:9800"))
        .await
        .map_err(|err| ErrorArrayItem::from(err))?;

    // Starting unix listener
    let unix_listener: UnixListener = match aggregator.exists() {
        true => {
            del_file(aggregator).uf_unwrap()?;
            UnixListener::bind(aggregator).map_err(ErrorArrayItem::from)
        }
        false => UnixListener::bind(aggregator).map_err(ErrorArrayItem::from),
    }?;

    // Monitor for timed out applications
    let store = app_status_store.clone();
    let execution_clone = execution.clone();
    let update_time_store = app_update_time_store.clone();
    tokio::spawn(async move {
        if let Err(err) = process_app_status(store, execution_clone, update_time_store).await {
            log!(LogLevel::Error, "{}", err);
        }
    });

    let portal_setup = portal_setup.clone();
    let portal_found = portal_found.clone();
    tokio::spawn(async move {
        loop {
            if portal_setup.load(Ordering::Relaxed) {
                log!(LogLevel::Info, "Registered with portal");
                break;
            }

            sleep(Duration::from_secs(10)).await;

            let portal_addr: Stringy = match get_portal_addr(&config).await {
                Ok(addr) => Stringy::from(addr),
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "Exiting portal registration: {}",
                        err.err_mesg
                    );
                    break;
                }
            };

            log!(LogLevel::Debug, "server located at {}", portal_addr);

            let result: Result<Result<(), ErrorArrayItem>, tokio::time::error::Elapsed> =
                timeout(Duration::from_secs(2), query_portal(portal_addr.clone())).await;

            match result {
                Ok(query_result) => match query_result {
                    Ok(_) => {
                        portal_found.store(true, Ordering::Relaxed);
                    }
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "Failed to validate the portal. Trying again"
                        );
                        log!(LogLevel::Trace, "Error details: {}", err);
                        portal_found.store(false, Ordering::Relaxed);
                    }
                },
                Err(err) => {
                    log!(LogLevel::Error, "Timeout occurred: {}. trying again", err);
                }
            }

            if portal_found.load(Ordering::Relaxed) {
                if let Ok(id) = Identifier::load_from_file() {
                    log!(LogLevel::Info, "identity loaded:  {}", id.id);
                    if let Err(err) =
                        register_with_portal(portal_addr.to_string(), id, &config).await
                    {
                        log!(LogLevel::Error, "Failed to register with portal: {}", err);
                    }
                    portal_setup.store(true, Ordering::Relaxed);
                }
            }
        }
    });

    let store = app_status_store.clone();
    let systemd = app_systemd_store.clone();
    let execution_clone = execution.clone();
    tokio::spawn(async move {
        loop {
            execution_clone.wait_if_paused().await;
            execution_clone.pause();

            let mut app_status_store_write_lock: tokio::sync::RwLockWriteGuard<
                '_,
                HashMap<Stringy, AppStatus>,
            > = match store.try_write().await {
                Ok(val) => val,
                Err(err) => {
                    log!(LogLevel::Error, "continueing: {}", err);
                    continue;
                }
            };

            let mut app_systemd_store_write_lock: tokio::sync::RwLockWriteGuard<
                '_,
                HashMap<Stringy, bool>,
            > = match systemd.try_write().await {
                Ok(val) => val,
                Err(err) => {
                    log!(LogLevel::Error, "continueing: {}", err);
                    continue;
                }                
            };
            
            for app in app_status_store_write_lock.iter_mut() {
                match SystemdService::new(&app.1.app_id) {
                    Ok(service) => {
                        app_systemd_store_write_lock.insert(app.0.clone(), true);
                        if let Ok(val) = service.is_active() {
                            if val {
                                if app.1.status == Status::Stopped {
                                    app.1.status = Status::Running
                                }
                            }
                        }
                    },
                    Err(err) => {
                        log!(LogLevel::Trace, "{}", err);
                        app_systemd_store_write_lock.insert(app.0.clone(), false);
                    },
                }
            }

            drop(app_systemd_store_write_lock);
            drop(app_status_store_write_lock);

            execution_clone.resume();
            sleep(Duration::from_secs(10)).await;
        }
    });

    // Listeners and reloads
    loop {
        tokio::select! {
            Ok(conn) = tcp_listener.accept() => {
                let reload_clone = reload_flag.clone();
                let execution_clone = execution.clone();
                let app_status_store = app_status_store.clone();
                state.event_counter += 1;
                update_state(&mut state, &state_path).await;

                tokio::spawn(async move {
                    if let Err(err) = process_tcp(conn, reload_clone, execution_clone, app_status_store).await {
                        log!(LogLevel::Error, "TCP connection handling panicked: {:?}", err);
                    }
                });
            }

            Ok(conn) = unix_listener.accept() => {
                let execution_clone = execution.clone();
                let app_status_store = app_status_store.clone();
                let update_time_store = app_update_time_store.clone();
                state.event_counter += 1;
                update_state(&mut state, &state_path).await;

                tokio::spawn(async move {
                    if let Err(err) = process_unix(conn, execution_clone, app_status_store, update_time_store).await {
                        log!(LogLevel::Error, "UNIX socket handling panicked: {:?}", err);
                    }
                });
            }

            _ = reload_flag.notified() => {
                match execution.wait_with_timeout(Duration::from_secs(600)).await {
                    Ok(_) => {
                        execution.pause();
                        sleep(Duration::from_secs(1)).await;

                        log!(LogLevel::Info, "Reloading");
                        state.event_counter += 1;
                        update_state(&mut state, &state_path).await;
                        let store_clone = app_status_store.clone();

                        let mut store_lock = match store_clone
                        .try_write_with_timeout(Some(Duration::from_secs(30)))
                        .await{
                            Ok(data) => {
                                log!(LogLevel::Trace, "got data from guard !");
                                data
                            },
                            Err(err) => {
                                log!(LogLevel::Error, "Error reloading: {}, Doing it the rough way", err);
                                std::process::exit(100)
                            },
                        };

                        trim(app_status_store.clone()).await?;
                        log!(LogLevel::Trace, "Trimmed!");


                        // Clearing and reloading the internal store
                        let apps_hash = store_lock.iter();
                        log!(LogLevel::Trace, "Turned data into iter!");
                        let mut app_vec = Vec::new();

                        for app_hash in apps_hash {
                            app_vec.push(app_hash.1.to_owned());
                        }

                        save_registered_apps(&app_vec).await?;
                        log!(LogLevel::Trace, "Saved to fs!");

                        store_lock.clear();
                        store_lock.shrink_to_fit();
                        log!(LogLevel::Trace, "Emptied and resized the store lock!");

                        let apps = load_registered_apps().await?;

                        for app in apps {
                            let app_clone = app.clone();
                            store_lock.insert(app_clone.app_id, app);
                        }
                        log!(LogLevel::Trace, "Refilled the store lock!");

                        drop(store_lock);


                        log!(LogLevel::Trace, "Starting state reload!");
                        // wind down then re initialize the state
                        wind_down_state(&mut state, &state_path).await;
                        let new_config = get_config();
                        state = match StatePersistence::load_state(&state_path).await {
                            Ok(data) => data,
                            Err(err) => {
                                log!(LogLevel::Error, "{}", err);
                                log!(LogLevel::Warn, "Aggregator reloaded, application state erroneous");
                                continue
                            },
                        };
                        state.config = new_config;
                        state.error_log.clear();
                        log!(LogLevel::Info, "Reloaded!");


                        execution.resume();
                    },
                    Err(err) => {
                        execution.resume();
                        log!(LogLevel::Error, "Couldn't get an execution lock in 5 mins. I'm outta here ~: {}", err);
                        continue
                    },
                }
            }

            _ = shutdown_flag.notified() => {
                log!(LogLevel::Info, "Shutting down");
                if !execution.is_paused().await {
                    execution.pause();
                }
                sleep(Duration::from_secs(3)).await;

                wind_down_state(&mut state, &state_path).await;
                std::process::exit(0);
            }

            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                execution.wait_if_paused().await;
                sleep(Duration::from_millis(100)).await;
                if let Err(err) = update_uptime(app_status_store.clone(), app_update_time_store.clone()).await {
                    log!(LogLevel::Error, "{}", err);
                }            }
        }
    }
}

fn register_self(config: &AppConfig) -> (Stringy, AppStatus) {
    let status = AppStatus {
        app_id: config.app_name.clone(),
        status: Status::Running,
        uptime: None,
        error: None,
        metrics: None,
        timestamp: current_timestamp(),
        expected_status: Status::Running,
        system_application: true,
    };

    return (config.app_name.clone(), status);
}

async fn register_app(
    reg_app: &RegisterApp,
    app_status_store: AppStatusStore,
    app_update_time_store: AppUpdateTimeStore,
) -> Result<(), ErrorArrayItem> {
    // let app_state_store: LockWithTimeout<HashMap<Stringy, AppState>> =
    let app: AppStatus = AppStatus {
        app_id: reg_app.app_id.clone(),
        status: Status::Unknown,
        uptime: None,
        error: None,
        metrics: None,
        timestamp: reg_app.registration_timestamp,
        expected_status: reg_app.expected_status.clone(),
        system_application: reg_app.system_application,
    };

    let mut app_status_store_write_lock = app_status_store
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;

    let mut app_update_time_store_write_lock = app_update_time_store
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;

    match app_status_store_write_lock.insert(app.app_id.clone(), app.clone()) {
        Some(_) => {
            log!(LogLevel::Debug, "App was already registered");
        }
        None => {
            log!(LogLevel::Info, "App \"{}\" Registerd", reg_app.app_id);
            app_update_time_store_write_lock.insert(app.app_id, reg_app.registration_timestamp);
        }
    };

    drop(app_status_store_write_lock);
    drop(app_update_time_store_write_lock);

    Ok(())
}

async fn deregister_app(
    dreg_app: &DeregisterApp,
    app_status_store: AppStatusStore,
) -> Result<(), ErrorArrayItem> {
    let store_clone = app_status_store.clone();
    let mut store_lock = store_clone
        .try_write_with_timeout(Some(Duration::from_secs(3)))
        .await?;

    if store_lock.contains_key(&dreg_app.app_id) {
        if let Some(app) = store_lock.remove_entry(&dreg_app.app_id) {
            log!(LogLevel::Info, "App {} deregistered", app.0);
            log!(LogLevel::Trace, "App {} deregistered", app.1);
        }
    }

    drop(store_lock);

    Ok(())
}

// snagged old state update function to avoid recursive calls to aggragator updates:
// Update state and persist it to disk
async fn update_state(state: &mut AppState, path: &PathType) {
    state.last_updated = current_timestamp();
    state.event_counter += 1;
    if let Err(err) = StatePersistence::save_state(state, path).await {
        log!(LogLevel::Error, "Failed to save state: {}", err);
        state.is_active = false;
        state.error_log.push(ErrorArrayItem::new(
            Errors::GeneralError,
            format!("{}", err),
        ));
    }
}

// Update the state file in the case of a un handled error
pub async fn wind_down_state(state: &mut AppState, state_path: &PathType) {
    state.is_active = false;
    state.data = String::from("Terminated");
    state.last_updated = current_timestamp();
    state.error_log.push(ErrorArrayItem::new(
        Errors::GeneralError,
        "Wind down requested check logs".to_owned(),
    ));
    update_state(state, &state_path).await;
}
