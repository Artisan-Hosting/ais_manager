use std::{collections::HashMap, time::Duration};

use artisan_middleware::dusa_collection_utils::log::LogLevel;
use artisan_middleware::dusa_collection_utils::rwarc::LockWithTimeout;
use artisan_middleware::dusa_collection_utils::types::PathType;
use artisan_middleware::dusa_collection_utils::{errors::ErrorArrayItem, stringy::Stringy};
use artisan_middleware::dusa_collection_utils::{
    errors::Errors, functions::current_timestamp, log,
};
use artisan_middleware::{
    aggregator::{AppStatus, Status},
    process_manager::{spawn_complex_process, SupervisedChild, SupervisedProcess},
    state_persistence::AppState,
};
use once_cell::sync::Lazy;
use tokio::process::Command;

use crate::system::state::save_state;

use super::{
    pid::reclaim_child,
    resolve::{Applications, ClientApplication, SystemApplication},
};

pub enum SupervisedProcesses {
    Child(SupervisedChild),
    Process(SupervisedProcess),
}

pub static APP_STATUS_ARRAY: Lazy<LockWithTimeout<HashMap<Stringy, AppStatus>>> =
    Lazy::new(|| LockWithTimeout::new(HashMap::new()));
pub static SYSTEM_APPLICATION_HANDLER: Lazy<LockWithTimeout<HashMap<String, SupervisedProcesses>>> =
    Lazy::new(|| LockWithTimeout::new(HashMap::new()));
pub static CLIENT_APPLICATION_HANDLER: Lazy<LockWithTimeout<HashMap<String, SupervisedProcesses>>> =
    Lazy::new(|| LockWithTimeout::new(HashMap::new()));

pub async fn spawn_system_applications(
    system_application_handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>,
    applications: Vec<SystemApplication>,
    state: &mut AppState,
    state_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    for system_app in applications {
        if !system_app.exists {
            log!(
                LogLevel::Trace,
                "Binary path not found for: {}, Skipping...",
                system_app.name
            );
            continue;
        }

        // check if the application is running in a previous life
        let process = if let Some(app_state) = system_app.state {
            match reclaim_child(app_state.pid).await {
                Ok(process) => Some(process),
                Err(err) => {
                    if err.err_type == Errors::SupervisedChild {
                        log!(
                            LogLevel::Trace,
                            "{} not currently running, Spawning",
                            &system_app.name
                        );
                        None
                    } else {
                        return Err(err);
                    }
                }
            }
        } else {
            log!(LogLevel::Trace, "{} has no state file", &system_app.name);
            None
        };

        let system_process: SupervisedProcesses = match process {
            Some(proc) => {
                proc.monitor_usage().await;
                SupervisedProcesses::Process(proc)
            }
            None => {
                let mut command: Command = Command::new(system_app.path);
                let config_path: PathType = PathType::Content(format!("/etc/{}/", system_app.name));

                let system_child: SupervisedChild = match spawn_complex_process(
                    &mut command,
                    Some(config_path.clone()),
                    true,
                    true,
                )
                .await
                {
                    Ok(child) => {
                        log!(
                            LogLevel::Info,
                            "Started: {}:{}",
                            system_app.name,
                            child.get_pid().await.unwrap()
                        );
                        state.data = format!(
                            "{} started, with working dir: {}",
                            system_app.name, config_path
                        );
                        state.event_counter += 1;
                        save_state(state, state_path).await;
                        child
                    }
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "Failed to spawn: {}: {}",
                            system_app.name,
                            err
                        );
                        continue;
                    }
                };

                // saving pid info
                state.pid = system_child.get_pid().await?;

                system_child.monitor_usage().await;
                SupervisedProcesses::Child(system_child)
            }
        };

        // pushing application into the write lock
        let mut system_application_handler_write_lock =
            system_application_handler.try_write().await?;
        system_application_handler_write_lock.insert(system_app.name, system_process);
        drop(system_application_handler_write_lock);
    }

    state.data = format!("All system applications spawned");
    state.event_counter += 1;
    save_state(state, state_path).await;

    Ok(())
}

// TODO add pid tracking to assume control of running instances
pub async fn spawn_client_applications(
    client_application_handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>,
    applications: Vec<ClientApplication>,
    state: &mut AppState,
    state_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Info, "Spawning client applications");
    for client_app in applications {
        if !client_app.exists {
            log!(
                LogLevel::Trace,
                "Binary path not found for: {}, Skipping...",
                client_app.name
            );
            continue;
        }

        // check if the application is running in a previous life
        let process: Option<SupervisedProcesses> = if let Some(app_state) = client_app.state {
            match reclaim_child(app_state.pid).await {
                Ok(child_process) => {
                    log!(
                        LogLevel::Info,
                        "{} is already running with PID {}",
                        client_app.name,
                        app_state.pid
                    );
                    Some(SupervisedProcesses::Process(child_process)) // Wrap the reclaimed process
                }
                Err(err) => {
                    if err.err_type == Errors::SupervisedChild {
                        log!(
                            LogLevel::Trace,
                            "{} not currently running, spawning a new process",
                            client_app.name
                        );
                        None // Explicitly indicate that the process needs to be spawned
                    } else {
                        return Err(err); // Propagate unexpected errors
                    }
                }
            }
        } else {
            log!(
                LogLevel::Trace,
                "{} has no state file, spawning a new process",
                client_app.name
            );
            None // No state, so the process will need to be spawned
        };
        

        // TODO this is where we load and validate the environmental file and source the initlal data
        let client_child: SupervisedProcesses = match process {
            Some(existing_process) => existing_process, // Use the reclaimed process
            None => {
                // Prepare the command and spawn a new process
                let mut stub = Command::new(client_app.path);
                let command: &mut Command = match client_app.environ {
                    Some(env) => {
                        log!(
                            LogLevel::Info,
                            "Reading environmental file for: {}",
                            client_app.name
                        );
        
                        let uid_or_default = env.execution_uid.unwrap_or(33);
                        stub.gid(uid_or_default.into()).uid(uid_or_default.into());
        
                        if let Some(path_mod) = env.path_modifier {
                            stub.env("PATH", path_mod.to_string());
                        }
        
                        &mut stub
                    }
                    None => {
                        stub.env("NVM_DIR", "/var/www/.nvm") // Set NVM_DIR
                            .env(
                                "PATH",
                                "/var/www/.nvm/versions/node/v23.5.0/bin:/usr/local/bin:/usr/bin:/bin",
                            )
                    }
                };
        
                let config_path: PathType = PathType::Content(format!("/etc/{}/", client_app.name));
                match spawn_complex_process(command, Some(config_path), false, true).await {
                    Ok(child) => {
                        log!(
                            LogLevel::Info,
                            "Started: {} with PID {}",
                            client_app.name,
                            child.get_pid().await.unwrap_or_default()
                        );
                        SupervisedProcesses::Child(child)
                    }
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "Failed to spawn {}: {}",
                            client_app.name,
                            err
                        );
                        continue; // Skip this client application
                    }
                }
            }
        };
        
        // pushing application into the write lock
        let mut client_application_handler_write_lock =
            client_application_handler.try_write().await?;
        client_application_handler_write_lock
            .insert(client_app.name, client_child);
        drop(client_application_handler_write_lock);
    }

    state.data = format!("All client applications spawned");
    state.event_counter += 1;
    save_state(state, state_path).await;

    Ok(())
}

pub async fn populate_initial_state_lock(applications: Applications) -> Result<(), ErrorArrayItem> {
    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;

    let app_states: Vec<(String, Option<AppState>, bool)> = match applications {
        Applications::System(vec) => {
            let mut app_states = Vec::new();
            for app in vec {
                if let Some(state) = app.state {
                    app_states.push((app.name, Some(state), true));
                } else {
                    log!(
                        LogLevel::Trace,
                        "Skipping: {}, no state file found",
                        app.name
                    );
                    app_states.push((app.name, None, true));
                }
            }
            app_states
        }
        Applications::Client(vec) => {
            let mut app_states = Vec::new();
            for app in vec {
                if let Some(state) = app.state {
                    app_states.push((app.name, Some(state), false));
                } else {
                    log!(
                        LogLevel::Trace,
                        "Skipping: {}, no state file found",
                        app.name
                    );
                    app_states.push((app.name, None, false));
                }
            }
            app_states
        }
    };

    for app in app_states {
        let status = match app.1 {
            Some(state) => AppStatus {
                app_id: Stringy::from(&state.name),
                uptime: Some(current_timestamp()),
                error: if !state.error_log.is_empty() {
                    Some(state.error_log)
                } else {
                    None
                },
                metrics: None,
                timestamp: current_timestamp(),
                expected_status: Status::Running,
                system_application: app.2,
                status: Status::Running,
            },
            None => AppStatus {
                app_id: Stringy::from(app.0),
                status: Status::Unknown,
                uptime: None,
                error: Some(vec![ErrorArrayItem::new(
                    Errors::NotFound,
                    "Binary or state file not found",
                )]),
                metrics: None,
                timestamp: current_timestamp(),
                expected_status: Status::Unknown,
                system_application: app.2,
            },
        };

        application_status_array_write_lock.insert(Stringy::from(&*status.app_id), status);
    }

    Ok(())
}
