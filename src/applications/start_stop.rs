use std::io;

use artisan_middleware::aggregator::AppStatus;
use artisan_middleware::dusa_collection_utils::errors::{ErrorArrayItem, Errors};
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::types::pathtype::PathType;
use artisan_middleware::dusa_collection_utils::types::stringy::Stringy;
use artisan_middleware::systemd::SystemdService;
use artisan_middleware::{aggregator::Status, config::AppConfig, state_persistence::AppState};
use nix::libc::kill;

use crate::applications::child::populate_initial_state_lock;
use crate::applications::{
    child::{
        SupervisedProcesses, APP_STATUS_ARRAY, CLIENT_APPLICATION_HANDLER,
        SYSTEM_APPLICATION_HANDLER,
    },
    resolve::{resolve_client_applications, resolve_system_applications},
};
use crate::system::state::save_state;

use super::child::{spawn_single_application, CLIENT_APPLICATION_ARRAY, SYSTEM_APPLICATION_ARRAY};
use super::resolve::Application;

pub async fn stop_application(app_id: &Stringy) -> Result<(), ErrorArrayItem> {
    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let app_status = match app_status_array_write_lock.get_mut(&app_id) {
        Some(app) => {
            app.app_data.set_status(Status::Stopping);
            Some(app)
        }
        None => None, // couldn't find
    };

    match app_status {
        Some(app) => {
            // Determine if it's a system app
            let child: Option<SupervisedProcesses> = if app.app_data.is_system_application() {
                let mut lock = SYSTEM_APPLICATION_HANDLER.try_write().await?;
                log!(
                    LogLevel::Trace,
                    "{} Dropped from handler for termination",
                    app.app_id
                );
                lock.remove(&app.app_data.get_name().into())
            } else {
                let mut lock = CLIENT_APPLICATION_HANDLER.try_write().await?;
                log!(
                    LogLevel::Trace,
                    "{} Dropped from handler for termination",
                    app.app_id
                );
                lock.remove(&app.app_data.get_name().into())
            };

            if let Some(child) = child {
                match child {
                    SupervisedProcesses::Child(supervised_child) => {
                        app.app_data.set_status(Status::Stopped);
                        app.metrics = None;
                        app.uptime = None;
                        drop(app_status_array_write_lock);
                        send_stop(supervised_child.get_pid().await? as i32)?;
                        return Ok(());
                    }
                    SupervisedProcesses::Process(supervised_process) => {
                        app.app_data.set_status(Status::Stopped);
                        app.metrics = None;
                        app.uptime = None;
                        drop(app_status_array_write_lock);
                        send_stop(supervised_process.get_pid())?;
                        return Ok(());
                    }
                }
            } else {
                log!(
                    LogLevel::Warn,
                    "{} is in the app_array but not in a handler, errouneous state",
                    app_id
                );
                return Ok(());
            }
        }
        None => {
            log!(LogLevel::Warn, "{}, Not registered in the system", app_id);
            return Err(ErrorArrayItem::new(
                Errors::NotFound,
                format!("{}, Not registered in the system", app_id),
            ));
        }
    }
}

fn send_stop(pid: i32) -> Result<(), ErrorArrayItem> {
    // SIGUSR1 = 10
    let result: i32 = unsafe { kill(pid, 9) };

    if result == 0 {
        return Ok(());
    } else {
        let error: io::Error = io::Error::from_raw_os_error(result);
        return Err(ErrorArrayItem::from(error));
    }
}

pub async fn reload_application(app_id: &Stringy) -> Result<(), ErrorArrayItem> {
    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let app_status = match app_status_array_write_lock.get_mut(&app_id) {
        Some(app) => {
            app.app_data.set_status(Status::Stopping);
            Some(app)
        }
        None => None, // couldn't find
    };

    match app_status {
        Some(app) => {
            let lock = CLIENT_APPLICATION_HANDLER.try_read().await?;
            if let Some(child) = lock.get(&app.app_id) {
                match child {
                    SupervisedProcesses::Child(supervised_child) => {
                        let pid = supervised_child.get_pid().await?;
                        send_reload(pid as i32)?;
                        return Ok(());
                    }
                    SupervisedProcesses::Process(supervised_process) => {
                        let pid = supervised_process.get_pid();
                        send_reload(pid)?;
                        return Ok(());
                    }
                }
            };

            let lock = SYSTEM_APPLICATION_HANDLER.try_read().await?;
            if let Some(child) = lock.get(&app.app_id) {
                match child {
                    SupervisedProcesses::Child(supervised_child) => {
                        let pid = supervised_child.get_pid().await?;
                        send_reload(pid as i32)?;
                        return Ok(());
                    }
                    SupervisedProcesses::Process(supervised_process) => {
                        let pid = supervised_process.get_pid();
                        send_reload(pid)?;
                        return Ok(());
                    }
                }
            };

            return Err(ErrorArrayItem::new(
                Errors::NotFound,
                format!("{}, Not registered in the system", app_id),
            ));
        }
        None => {
            log!(LogLevel::Warn, "{}, Not registered in the system", app_id);
            return Err(ErrorArrayItem::new(
                Errors::NotFound,
                format!("{}, Not registered in the system", app_id),
            ));
        }
    }
}

fn send_reload(pid: i32) -> Result<(), ErrorArrayItem> {
    // SIGHUP = 1
    let result: i32 = unsafe { kill(pid, 1) };

    if result == 0 {
        return Ok(());
    } else {
        let error: io::Error = io::Error::from_raw_os_error(result);
        return Err(ErrorArrayItem::from(error));
    }
}

pub async fn start_application(
    app_id: &Stringy,
    _state: &mut AppState,
    _state_path: &PathType,
    _config: &AppConfig,
) -> Result<(), ErrorArrayItem> {
    let mut app_status_array_write_lock = APP_STATUS_ARRAY.try_write().await?;

    // Retrieve or initialize app status
    let app: &mut AppStatus = match app_status_array_write_lock.get_mut(app_id) {
        Some(app) => {
            app.app_data.set_status(Status::Starting);
            app
        }
        None => {
            let error = ErrorArrayItem::new(
                Errors::NotFound,
                format!("State data for: {} not loaded", app_id),
            );
            return Err(error);
        }
    };

    let systemd_app: SystemdService = match SystemdService::new(&app.app_data.get_name()) {
        Ok(systemd) => systemd,
        Err(err) => {
            return Err(ErrorArrayItem::from(err));
        }
    };

    let _is_active = match systemd_app.is_active() {
        Ok(b) => b,
        Err(err) => {
            return Err(ErrorArrayItem::new(Errors::NotFound, err.to_string()));
        }
    };

    // TODO ensure we kill all instances before starting a new one
    // match is_active {
    //     true => ,
    //     false => {
    //         if let Err(err) = systemd_app.start() {

    //         }
    //     },
    // }

    if let Err(err) = systemd_app.start() {
        return Err(ErrorArrayItem::new(Errors::Unauthorized, err.to_string()));
    }

    Ok(())

    // // Attempt to start the application
    // let app_started = if let Some(ref app) = app_status {
    //     if app.app_data.is_system_application() {
    //         start_system_application(app_id, state, state_path).await?
    //     } else if config.environment != "systemonly" {
    //         start_client_application(app_id, config, state, state_path).await?
    //     } else {
    //         log!(
    //             LogLevel::Warn,
    //             "Tried to start a client application in safe mode"
    //         );
    //         return Ok(());
    //     }
    // } else {
    //     let sys_started = start_system_application(app_id, state, state_path).await?;
    //     let cli_started = if config.environment != "systemonly" {
    //         start_client_application(app_id, config, state, state_path).await?
    //     } else {
    //         false
    //     };
    //     sys_started || cli_started
    // };

    // // Update application status and handle the result
    // if app_started {
    //     if let Some(app) = app_status {
    //         app.app_data.set_status(Status::Running);
    //         app.timestamp = current_timestamp();
    //     }
    //     Ok(())
    // } else {
    //     Err(ErrorArrayItem::new(
    //         Errors::NotFound,
    //         format!("{}, Not found on the system", app_id),
    //     ))
    // }
}

/// Helper to start system applications
async fn _start_system_application(
    app_id: &Stringy,
    state: &mut AppState,
    state_path: &PathType,
) -> Result<bool, ErrorArrayItem> {
    // update the system application index
    resolve_system_applications().await?;

    for sys_app in SYSTEM_APPLICATION_ARRAY
        .try_read()
        .await?
        .clone()
        .into_iter()
    {
        if app_id == &sys_app.0 {
            spawn_single_application(Application::System(sys_app.1), state, state_path).await?;
            populate_initial_state_lock(state).await?;
            save_state(state, state_path).await;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Helper to start client applications
async fn _start_client_application(
    app_id: &Stringy,
    config: &AppConfig,
    state: &mut AppState,
    state_path: &PathType,
) -> Result<bool, ErrorArrayItem> {
    // updating the client application index
    resolve_client_applications(config).await?;

    for cli_app in CLIENT_APPLICATION_ARRAY
        .try_read()
        .await?
        .clone()
        .into_iter()
    {
        if app_id == &cli_app.0 {
            spawn_single_application(Application::Client(cli_app.1), state, state_path).await?;
            populate_initial_state_lock(state).await?;
            save_state(state, state_path).await;
            return Ok(true);
        }
    }
    Ok(false)
}
