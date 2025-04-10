use std::io;
use std::time::Duration;

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
            if app.app_data.is_system_application() {
                let mut lock = SYSTEM_APPLICATION_HANDLER.try_write().await?;
                log!(
                    LogLevel::Trace,
                    "{} Dropped from handler for termination",
                    app.app_id
                );

                if let None = lock.remove(&app.app_data.get_name().into()) {
                    log!(
                        LogLevel::Warn,
                        "The application wasn't registered with the handler: {}",
                        app.app_id
                    );
                }
            } else {
                let mut lock = CLIENT_APPLICATION_HANDLER.try_write().await?;
                log!(
                    LogLevel::Trace,
                    "{} Dropped from handler for termination",
                    app.app_id
                );

                if let None = lock.remove(&app.app_data.get_name().into()) {
                    log!(
                        LogLevel::Warn,
                        "The application wasn't registered with the handler: {}",
                        app.app_id
                    );
                }
            };

            app.app_data.set_status(Status::Stopped);
            app.metrics = None;
            app.uptime = None;
            send_stop(&app)?;
            // drop(app_status_array_write_lock);
            return Ok(());
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

fn send_stop(app: &AppStatus) -> Result<(), ErrorArrayItem> {
    let systemd_app = SystemdService::new(&app.app_data.get_name())?;

    systemd_app.kill().map_err(|err| {
        ErrorArrayItem::new(
            Errors::GeneralError,
            format!("Failed to kill application: {}", err.to_string()),
        )
    })?;

    systemd_app
        .is_active()
        .map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!(
                    "Error checking if system service is active: {}",
                    err.to_string()
                ),
            )
        })
        .map(|active| match active {
            true => {
                let kill_result: i32 = unsafe { kill(app.app_data.get_pid() as i32, 9) };
                if kill_result != 0 {
                    let error: io::Error = io::Error::from_raw_os_error(kill_result);
                    Err(ErrorArrayItem::from(error))
                } else {
                    Ok(())
                }
            }
            false => Ok(()),
        })?
    // SIGUSR1 = 10
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
    let app_status_array_read_lock = APP_STATUS_ARRAY
        .try_read_with_timeout(Some(Duration::from_secs(20)))
        .await?;

    // Retrieve or initialize app status
    let app: &AppStatus = match app_status_array_read_lock.get(app_id) {
        Some(app) => app,
        None => {
            let error = ErrorArrayItem::new(
                Errors::NotFound,
                format!("State data for: {} not loaded", app_id),
            );
            return Err(error);
        }
    };

    let systemd_app = SystemdService::new(&app.app_data.get_name())?;

    systemd_app
        .is_active()
        .map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!(
                    "Error checking if system service is active: {}",
                    err.to_string()
                ),
            )
        })
        .map(|active| {
            match active {
                true => send_stop(app),
                false => {
                    if let Err(err) = systemd_app.start() {
                        Err(ErrorArrayItem::new(Errors::Unauthorized, err.to_string()))
                    } else {
                        Ok(())
                    }
                }
            }
        })?
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
