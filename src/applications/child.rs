use artisan_middleware::config_bundle::ApplicationConfig;
use artisan_middleware::dusa_collection_utils::errors::ErrorArrayItem;
use artisan_middleware::dusa_collection_utils::functions::{create_hash, truncate};
use artisan_middleware::dusa_collection_utils::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::types::pathtype::PathType;
use artisan_middleware::dusa_collection_utils::types::rwarc::LockWithTimeout;
use artisan_middleware::dusa_collection_utils::types::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::{errors::Errors, log};
use artisan_middleware::enviornment::definitions::Enviornment;
use artisan_middleware::identity::Identifier;
use artisan_middleware::{
    aggregator::{AppStatus, Status},
    process_manager::{spawn_complex_process, SupervisedChild, SupervisedProcess},
    state_persistence::AppState,
};
use once_cell::sync::Lazy;
use std::fs;
use std::io::{BufRead, BufReader};
use std::{collections::HashMap, time::Duration};
use tokio::process::Command;

use crate::system::state::save_state;

use super::resolve::Application;
use super::{
    pid::reclaim_child,
    resolve::{ClientApplication, SystemApplication},
};

pub enum SupervisedProcesses {
    Child(SupervisedChild),
    Process(SupervisedProcess),
}

pub static APP_STATUS_ARRAY: Lazy<LockWithTimeout<HashMap<Stringy, AppStatus>>> =
    Lazy::new(|| LockWithTimeout::new(HashMap::new()));

pub static SYSTEM_APPLICATION_HANDLER: Lazy<
    LockWithTimeout<HashMap<Stringy, SupervisedProcesses>>,
> = Lazy::new(|| LockWithTimeout::new(HashMap::new()));

pub static CLIENT_APPLICATION_HANDLER: Lazy<
    LockWithTimeout<HashMap<Stringy, SupervisedProcesses>>,
> = Lazy::new(|| LockWithTimeout::new(HashMap::new()));

pub static CLIENT_APPLICATION_ARRAY: Lazy<LockWithTimeout<HashMap<Stringy, ClientApplication>>> =
    Lazy::new(|| LockWithTimeout::new(HashMap::new()));

pub static SYSTEM_APPLICATION_ARRAY: Lazy<LockWithTimeout<HashMap<Stringy, SystemApplication>>> =
    Lazy::new(|| LockWithTimeout::new(HashMap::new()));

pub async fn _spawn_system_applications(
    system_application_handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>,
    system_application_array: LockWithTimeout<HashMap<String, SystemApplication>>,
    state: &mut AppState,
    state_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    let system_application_array_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, SystemApplication>,
    > = system_application_array.try_read().await?;

    for system_app in system_application_array_read_lock.clone().into_iter() {
        if !system_app.1.exists {
            log!(
                LogLevel::Trace,
                "Binary path not found for: {}, Skipping...",
                system_app.0
            );
            continue;
        }

        // check if the application is running in a previous life
        let process = match reclaim_child(system_app.1.config.get_pid()).await {
            Ok(process) => Some(process),
            Err(err) => {
                if err.err_type == Errors::SupervisedChild {
                    log!(
                        LogLevel::Trace,
                        "{} not currently running, Spawning",
                        &system_app.0
                    );
                    None
                } else {
                    return Err(err);
                }
            }
        };

        let system_process: SupervisedProcesses = match process {
            Some(mut proc) => {
                proc.monitor_usage().await;
                SupervisedProcesses::Process(proc)
            }
            None => {
                let mut command: Command = Command::new(system_app.1.path);
                let config_path: PathType = PathType::Content(format!("/etc/{}/", system_app.0));

                let mut system_child: SupervisedChild = match spawn_complex_process(
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
                            system_app.0,
                            child.get_pid().await.unwrap()
                        );
                        state.data = format!(
                            "{} started, with working dir: {}",
                            system_app.0, config_path
                        );
                        state.event_counter += 1;
                        save_state(state, state_path).await?;
                        child
                    }
                    Err(err) => {
                        log!(
                            LogLevel::Error,
                            "Failed to spawn: {}: {}",
                            system_app.0,
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
        system_application_handler_write_lock.insert(system_app.0, system_process);
        drop(system_application_handler_write_lock);
    }

    state.data = format!("All system applications spawned");
    state.event_counter += 1;
    save_state(state, state_path).await?;

    Ok(())
}

pub async fn _spawn_client_applications(
    client_application_handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>,
    client_application_array: LockWithTimeout<HashMap<String, ClientApplication>>,
    state: &mut AppState,
    state_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Info, "Spawning client applications");

    let client_application_array_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, ClientApplication>,
    > = client_application_array.try_read().await?;

    for client_app in client_application_array_read_lock.clone().into_iter() {
        if !client_app.1.exists {
            log!(
                LogLevel::Trace,
                "Binary path not found for: {}, Skipping...",
                client_app.0
            );
            continue;
        }

        // check if the application is running in a previous life
        let process: Option<SupervisedProcesses> =
            match reclaim_child(client_app.1.config.get_pid()).await {
                Ok(child_process) => {
                    log!(
                        LogLevel::Info,
                        "{} is already running with PID {}",
                        client_app.0,
                        client_app.1.config.get_pid()
                    );
                    Some(SupervisedProcesses::Process(child_process)) // Wrap the reclaimed process
                }
                Err(err) => {
                    if err.err_type == Errors::SupervisedChild {
                        log!(
                            LogLevel::Trace,
                            "{} not currently running, spawning a new process",
                            client_app.0
                        );
                        None // Explicitly indicate that the process needs to be spawned
                    } else {
                        return Err(err); // Propagate unexpected errors
                    }
                }
            };

        // TODO this is where we load and validate the environmental file and source the initlal data
        let client_child: SupervisedProcesses = match process {
            Some(existing_process) => existing_process, // Use the reclaimed process
            None => {
                // Prepare the command and spawn a new process
                let mut stub = Command::new(client_app.1.path);
                let command: &mut Command = match client_app.1.config.get_enviornmentals() {
                    Some(env) => {
                        stub = match env {
                            Enviornment::V1(enviornment_v1) => {
                                log!(
                                    LogLevel::Info,
                                    "Reading environmental file for: {}",
                                    client_app.0
                                );

                                let uid_or_default = enviornment_v1.execution_uid.unwrap_or(33);
                                stub.gid(uid_or_default.into()).uid(uid_or_default.into());

                                if let Some(path_mod) = enviornment_v1.path_modifier {
                                    stub.env("PATH", path_mod.to_string());
                                }

                                stub.env("NVM_DIR", "/var/www/.nvm"); // Set NVM_DIR

                                stub
                            }
                            Enviornment::V2(enviornment_v2) => {
                                log!(
                                    LogLevel::Info,
                                    "Reading environmental file for: {}",
                                    client_app.0
                                );

                                let uid_or_default = enviornment_v2.execution_uid.unwrap_or(33);
                                stub.gid(uid_or_default.into()).uid(uid_or_default.into());

                                if let Some(path_mod) = enviornment_v2.path_modifier {
                                    stub.env("PATH", path_mod.to_string());
                                }

                                stub.env("NVM_DIR", "/var/www/.nvm"); // Set NVM_DIR

                                // ! Actually implement the rest of V2

                                stub
                            }
                        };

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

                let config_path: PathType = PathType::Content(format!("/etc/{}/", client_app.0));
                match spawn_complex_process(command, Some(config_path), false, true).await {
                    Ok(child) => {
                        log!(
                            LogLevel::Info,
                            "Started: {} with PID {}",
                            client_app.0,
                            child.get_pid().await.unwrap_or_default()
                        );
                        SupervisedProcesses::Child(child)
                    }
                    Err(err) => {
                        log!(LogLevel::Error, "Failed to spawn {}: {}", client_app.0, err);
                        continue; // Skip this client application
                    }
                }
            }
        };

        // pushing application into the write lock
        let mut client_application_handler_write_lock =
            client_application_handler.try_write().await?;
        client_application_handler_write_lock.insert(client_app.0, client_child);
        drop(client_application_handler_write_lock);
    }

    state.data = format!("All client applications spawned");
    state.event_counter += 1;
    save_state(state, state_path).await?;

    Ok(())
}

#[allow(unused)]
pub async fn spawn_single_application(
    application: Application,
    state: &mut AppState,
    state_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    match application {
        Application::System(system_application) => {
            if !system_application.exists {
                log!(
                    LogLevel::Trace,
                    "Binary path not found for: {}, Skipping...",
                    system_application.name
                );
                return Ok(());
            }

            // check if the application is running in a previous life
            let process = match reclaim_child(system_application.config.get_pid()).await {
                Ok(process) => Some(process),
                Err(err) => {
                    if err.err_type == Errors::SupervisedChild {
                        log!(
                            LogLevel::Trace,
                            "{} not currently running, Spawning",
                            &system_application.name
                        );
                        None
                    } else {
                        return Err(err);
                    }
                }
            };

            let system_process: SupervisedProcesses = match process {
                Some(mut proc) => {
                    proc.monitor_usage().await;
                    SupervisedProcesses::Process(proc)
                }
                None => {
                    let mut command: Command = Command::new(system_application.path);
                    let config_path: PathType =
                        PathType::Content(format!("/etc/{}/", system_application.name));

                    let mut system_child: SupervisedChild = match spawn_complex_process(
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
                                system_application.name,
                                child.get_pid().await.unwrap()
                            );
                            state.data = format!(
                                "{} started, with working dir: {}",
                                system_application.name, config_path
                            );
                            state.event_counter += 1;
                            save_state(state, state_path).await;
                            child
                        }
                        Err(err) => {
                            log!(
                                LogLevel::Error,
                                "Failed to spawn: {}: {}",
                                system_application.name,
                                err
                            );
                            return Err(err);
                        }
                    };

                    // saving pid info
                    state.pid = system_child.get_pid().await?;

                    system_child.monitor_usage().await;
                    SupervisedProcesses::Child(system_child)
                }
            };

            // pushing application into the write lock
            let mut system_handler_write_lock: tokio::sync::RwLockWriteGuard<
                '_,
                HashMap<Stringy, SupervisedProcesses>,
            > = SYSTEM_APPLICATION_HANDLER
                .try_write()
                .await
                .map_err(|mut err| {
                    err.err_mesg = format!(
                        "Error getting write lock on system handler in spawn single application: {}",
                        err.err_mesg
                    )
                    .into();
                    err
                })?;

            system_handler_write_lock.insert(system_application.name, system_process);
            drop(system_handler_write_lock);

            return Ok(());
        }
        Application::Client(client_application) => {
            if !client_application.exists {
                log!(
                    LogLevel::Trace,
                    "Binary path not found for: {}, Skipping...",
                    client_application.name
                );
                return Ok(());
            }

            // check if the application is running in a previous life
            let process: Option<SupervisedProcesses> =
                match reclaim_child(client_application.config.get_pid()).await {
                    Ok(child_process) => {
                        log!(
                            LogLevel::Info,
                            "{} is already running with PID {}",
                            client_application.name,
                            client_application.config.get_pid()
                        );
                        Some(SupervisedProcesses::Process(child_process)) // Wrap the reclaimed process
                    }
                    Err(err) => {
                        if err.err_type == Errors::SupervisedChild {
                            log!(
                                LogLevel::Trace,
                                "{} not currently running, spawning a new process",
                                client_application.name
                            );
                            None // Explicitly indicate that the process needs to be spawned
                        } else {
                            return Err(err); // Propagate unexpected errors
                        }
                    }
                };

            let client_child: SupervisedProcesses = match process {
                Some(existing_process) => existing_process, // Use the reclaimed process
                None => {
                    // Prepare the command and spawn a new process
                    let mut stub = Command::new(client_application.path);
                    let command: &mut Command = match client_application.config.get_enviornmentals()
                    {
                        Some(env) => {
                            stub = match env {
                                Enviornment::V1(enviornment_v1) => {
                                    log!(
                                        LogLevel::Info,
                                        "Reading environmental file for: {}",
                                        client_application.name
                                    );

                                    let uid_or_default = enviornment_v1.execution_uid.unwrap_or(33);
                                    stub.gid(uid_or_default.into()).uid(uid_or_default.into());

                                    if let Some(path_mod) = enviornment_v1.path_modifier {
                                        stub.env("PATH", path_mod.to_string());
                                    }

                                    stub.env("NVM_DIR", "/var/www/.nvm"); // Set NVM_DIR

                                    stub
                                }
                                Enviornment::V2(enviornment_v2) => {
                                    log!(
                                        LogLevel::Info,
                                        "Reading environmental file for: {}",
                                        client_application.name
                                    );

                                    let uid_or_default = enviornment_v2.execution_uid.unwrap_or(33);
                                    stub.gid(uid_or_default.into()).uid(uid_or_default.into());

                                    if let Some(path_mod) = enviornment_v2.path_modifier {
                                        stub.env("PATH", path_mod.to_string());
                                    }

                                    stub.env("NVM_DIR", "/var/www/.nvm"); // Set NVM_DIR

                                    // ! Actually implement the rest of V2

                                    stub
                                }
                            };

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

                    let config_path: PathType =
                        PathType::Content(format!("/etc/{}/", client_application.name));
                    match spawn_complex_process(command, Some(config_path), false, true).await {
                        Ok(child) => {
                            log!(
                                LogLevel::Info,
                                "Started: {} with PID {}",
                                client_application.name,
                                child.get_pid().await.unwrap_or_default()
                            );
                            SupervisedProcesses::Child(child)
                        }
                        Err(err) => {
                            log!(
                                LogLevel::Error,
                                "Failed to spawn {}: {}",
                                client_application.name,
                                err
                            );
                            return Err(err);
                        }
                    }
                }
            };

            // pushing application into the write lock
            let mut client_application_handler_write_lock =
                CLIENT_APPLICATION_HANDLER.try_write().await.map_err(|mut err| {
                    err.err_mesg = format!(
                        "Error getting write lock on system handler in spawn single application: {}",
                        err.err_mesg
                    )
                    .into();
                    err
                })?;
            client_application_handler_write_lock
                .insert(client_application.name.into(), client_child);
            drop(client_application_handler_write_lock);

            return Ok(());
        }
    }
}

pub async fn populate_initial_state_lock(state: &mut AppState) -> Result<(), ErrorArrayItem> {
    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;

    let mut applications: Vec<Application> = Vec::new();
    let mut app_states: Vec<(Stringy, ApplicationConfig, bool)> = Vec::new();

    // working on the system applications
    {
        let system_application_array_read_lock = SYSTEM_APPLICATION_ARRAY.try_read().await?;
        for app in system_application_array_read_lock.clone().into_iter() {
            applications.push(Application::System(app.1));
        }
        state.data = "Re-populating system applications in status array".to_owned();
    }

    // add gate for client applications
    if state.config.environment != "systemonly" {
        let client_application_array_read_lock = CLIENT_APPLICATION_ARRAY.try_read().await?;

        for app in client_application_array_read_lock.clone().into_iter() {
            applications.push(Application::Client(app.1));
        }

        state.data = "Re-populating client applications in status array".to_owned();
    }

    for app in applications {
        match app {
            Application::System(system_application) => {
                app_states.push((
                    system_application.name,
                    system_application.config.clone(),
                    true,
                ));
            }
            Application::Client(client_application) => {
                app_states.push((
                    client_application.name,
                    client_application.config.clone(),
                    false,
                ));
            }
        }
    }

    for app in app_states {
        let identity: Identifier = Identifier::load_from_file()?;

        let app_id: Stringy = {
            let data = format!("{}-{}", identity.id, app.0);
            let hash = create_hash(data);
            truncate(&*hash, 20).to_owned()
        };

        let git_id: Stringy = {
            match app.1.is_system_application() {
                true => "".into(),
                false => app.0.replace("ais_", "").into(),
            }
        };

        let expected_status = {
            match app.1.is_system_application() {
                true => Status::Running,
                false => Status::Idle,
            }
        };

        let app_status: AppStatus = AppStatus {
            app_id,
            git_id,
            app_data: app.1.clone(),
            uptime: None,
            metrics: None,
            timestamp: app.1.state.stared_at,
            expected_status,
        };

        if let Some(old) = application_status_array_write_lock.insert(app.0, app_status.clone()) {
            log!(LogLevel::Debug, "Updated? {}", old.app_id)
        } else {
            log!(
                LogLevel::Debug,
                "Inserted? {}",
                app_status.app_data.get_name()
            );
        };
    }

    Ok(())
}

pub fn pids_in_cgroup(service_name: &str) -> std::io::Result<Vec<u32>> {
    let path = format!("/sys/fs/cgroup/system.slice/{}.service/cgroup.procs", service_name);
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    let pids = reader
        .lines()
        .filter_map(|line| line.ok())
        .filter_map(|line| line.parse::<u32>().ok())
        .collect();

    Ok(pids)
}
