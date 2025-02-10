use artisan_middleware::aggregator::{AppStatus, Metrics, Status};
use artisan_middleware::dusa_collection_utils::errors::Errors;
use artisan_middleware::dusa_collection_utils::functions::current_timestamp;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::types::rwarc::LockWithTimeout;
use artisan_middleware::dusa_collection_utils::types::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::{
    errors::ErrorArrayItem, logger::LogLevel,
};
use artisan_middleware::process_manager::is_pid_active;
use artisan_middleware::state_persistence::AppState;
use std::collections::{HashMap, HashSet};

use crate::applications::child::{
    CLIENT_APPLICATION_ARRAY, CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER,
};
use crate::applications::resolve::{
    resolve_client_applications, resolve_system_applications, SystemApplication,
};

use super::child::{SupervisedProcesses, APP_STATUS_ARRAY, SYSTEM_APPLICATION_ARRAY};
use super::pid::reclaim_child;
use super::resolve::ClientApplication;

pub async fn monitor_application_resource_usage(
    handler: LockWithTimeout<HashMap<Stringy, SupervisedProcesses>>,
) -> Result<(), ErrorArrayItem> {
    let application_handler_read_lock = handler.try_read().await?;

    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    for (name, app) in application_handler_read_lock.iter() {
        log!(LogLevel::Debug, "USAGE MONITOR: -> {}", name);
        match app {
            SupervisedProcesses::Child(supervised_child) => {
                // cheap fix
                if !supervised_child.running().await {
                    continue;
                }

                match supervised_child
                    .monitor
                    .0
                    .try_write_with_timeout(None)
                    .await
                {
                    Ok(mut monitor_lock) => {
                        let usage: (f32, f32) = monitor_lock.aggregate_tree_usage()?;
                        log!(
                            LogLevel::Debug,
                            "{} usage : cpu:{}, ram: {}",
                            supervised_child.get_pid().await?,
                            usage.0,
                            usage.1
                        );
                        monitor_lock.cpu = usage.0;
                        monitor_lock.ram = usage.1;

                        if let Some(app_arr_val) = app_status_array_write_lock.get_mut(&name) {
                            app_arr_val.metrics = Some(Metrics {
                                cpu_usage: usage.0,
                                memory_usage: usage.1,
                                other: None,
                            })
                        }
                    }
                    Err(err) => {
                        log!(LogLevel::Error, "Error locking monitor: {}", err);
                        break;
                    }
                }
            }
            SupervisedProcesses::Process(supervised_process) => {
                if !supervised_process.active() {
                    continue;
                }

                match supervised_process
                    .monitor
                    .0
                    .try_write_with_timeout(None)
                    .await
                {
                    Ok(mut monitor_lock) => {
                        let usage: (f32, f32) = monitor_lock.aggregate_tree_usage()?;
                        log!(
                            LogLevel::Debug,
                            "{} usage : cpu:{}, ram: {}",
                            supervised_process.get_pid(),
                            usage.0,
                            usage.1
                        );
                        monitor_lock.cpu = usage.0;
                        monitor_lock.ram = usage.1;

                        if let Some(app_arr_val) = app_status_array_write_lock.get_mut(&name) {
                            app_arr_val.metrics = Some(Metrics {
                                cpu_usage: usage.0,
                                memory_usage: usage.1,
                                other: None,
                            })
                        }
                    }
                    Err(err) => {
                        log!(LogLevel::Error, "Error locking monitor: {}", err);
                        break;
                    }
                }
            }
        };
    }

    Ok(())
}

pub async fn handle_dead_applications() -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Debug, "Handling dead applications");
    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let mut system_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, SupervisedProcesses>,
    > = SYSTEM_APPLICATION_HANDLER.try_write().await?;

    let mut system_handler_to_remove: HashSet<Stringy> = HashSet::new();

    let mut client_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, SupervisedProcesses>,
    > = CLIENT_APPLICATION_HANDLER.try_write().await?;

    let mut client_handler_to_remove: HashSet<Stringy> = HashSet::new();

    for system_handle in system_handler_write_lock.iter_mut() {
        let system_name: &Stringy = system_handle.0;
        if let Some(system_application_status) = app_status_array_write_lock.get_mut(&system_name) {
            match system_handle.1 {
                SupervisedProcesses::Child(supervised_child) => {
                    if !supervised_child.running().await {
                        // Set status properly
                        system_application_status
                            .app_data
                            .set_status(Status::Stopped);
                        system_application_status.metrics = None;
                        system_application_status.uptime = None;
                        system_application_status.timestamp = current_timestamp();

                        // put in to be removed set
                        system_handler_to_remove
                            .insert(system_application_status.app_data.get_name().into());

                        // Dropping the monitor
                        supervised_child.terminate_monitor();
                    }
                }
                SupervisedProcesses::Process(supervised_process) => {
                    if !supervised_process.active() {
                        // Set status properly
                        system_application_status
                            .app_data
                            .set_status(Status::Stopped);
                        system_application_status.metrics = None;
                        system_application_status.uptime = None;
                        system_application_status.timestamp = current_timestamp();

                        // put in to be removed set
                        system_handler_to_remove
                            .insert(system_application_status.app_data.get_name().into());

                        // Dropping the monitor
                        supervised_process.terminate_monitor();
                    }
                }
            }
        }
    }

    for client_handle in client_handler_write_lock.iter_mut() {
        let client_name: &Stringy = client_handle.0;
        if let Some(client_application_status) = app_status_array_write_lock.get_mut(&client_name) {
            match client_handle.1 {
                SupervisedProcesses::Child(supervised_child) => {
                    if !supervised_child.running().await {
                        // Set status properly
                        client_application_status
                            .app_data
                            .set_status(Status::Stopped);
                        client_application_status.metrics = None;
                        client_application_status.uptime = None;
                        client_application_status.timestamp = current_timestamp();

                        // put in to be removed set
                        client_handler_to_remove
                            .insert(client_application_status.app_data.get_name().into());

                        // Dropping the monitor
                        supervised_child.terminate_monitor();
                    }
                }
                SupervisedProcesses::Process(supervised_process) => {
                    if !supervised_process.active() {
                        // Set status properly
                        client_application_status
                            .app_data
                            .set_status(Status::Stopped);
                        client_application_status.metrics = None;
                        client_application_status.uptime = None;
                        client_application_status.timestamp = current_timestamp();

                        // put in to be removed set
                        client_handler_to_remove
                            .insert(client_application_status.app_data.get_name().into());

                        // Dropping the monitor
                        supervised_process.terminate_monitor();
                    }
                }
            }
        }
    }

    // removing system apps
    for id in system_handler_to_remove.iter() {
        if let Some(_) = system_handler_write_lock.remove(&id) {
            log!(LogLevel::Info, "Removed: {} from system handler", id);
        }
    }

    // removing client apps
    for id in client_handler_to_remove.iter() {
        if let Some(_) = client_handler_write_lock.remove(&id) {
            log!(LogLevel::Info, "Removed: {} from client handler", id);
        }
    }

    Ok(())
}

pub async fn handle_new_system_applications() -> Result<(), ErrorArrayItem> {
    // resolve current applications
    resolve_system_applications().await?;

    let mut system_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, SupervisedProcesses>,
    > = SYSTEM_APPLICATION_HANDLER.try_write().await?;

    let system_application_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<Stringy, crate::applications::resolve::SystemApplication>,
    > = SYSTEM_APPLICATION_ARRAY.try_read().await?;

    let mut system_to_start: HashMap<Stringy, SystemApplication> = HashMap::new();

    for new_app in system_application_read_lock.iter() {
        if !system_handler_write_lock.contains_key(new_app.0) {
            system_to_start.insert(new_app.0.clone(), new_app.1.clone());
        }
    }

    drop(system_application_read_lock);

    // Starting the applications.
    // TODO if system apps are started here, they more than likly failed with systemd
    // TODO Send a email or notification to check on this system if apps are running like this

    for id in system_to_start {
        // spawn_single_application(Application::System(id.1), &mut state, state_path).await?;
        // instead of spawning let's just try to reclaim the pid

        let app_state = id.clone().1.config;

        match reclaim_child(app_state.get_pid()).await {
            Ok(mut process) => {
                if !process.monitoring() {
                    process.monitor_usage().await;
                }

                // Updating the status array
                let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
                    '_,
                    HashMap<Stringy, AppStatus>,
                > = APP_STATUS_ARRAY.try_write().await?;

                if let Some(app) = app_status_array_write_lock.get_mut(&id.0) {
                    app.app_data.set_pid(process.get_pid() as u32);
                    app.app_data.set_status(app_state.get_status());
                    if app.app_data.get_status() == Status::Idle {
                        app.metrics = None;
                    }
                }

                // Adding to handler
                system_handler_write_lock
                    .insert(id.clone().0, SupervisedProcesses::Process(process));
                log!(
                    LogLevel::Info,
                    "{} Started and added to the system handler",
                    id.0
                );
            }
            Err(err) => {
                if err.err_type == Errors::SupervisedChild {
                    log!(LogLevel::Trace, "{} not currently running", id.0);
                    continue;
                } else {
                    return Err(err);
                }
            }
        }
    }

    Ok(())
}

pub async fn handle_new_client_applications(state: &mut AppState) -> Result<(), ErrorArrayItem> {
    // resolve current applications
    resolve_client_applications(&state.config).await?;

    let mut client_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, SupervisedProcesses>,
    > = CLIENT_APPLICATION_HANDLER.try_write().await?;

    let client_application_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<Stringy, crate::applications::resolve::ClientApplication>,
    > = CLIENT_APPLICATION_ARRAY.try_read().await?;

    let mut client_to_start: HashMap<Stringy, ClientApplication> = HashMap::new();

    for new_app in client_application_read_lock.iter() {
        if !client_handler_write_lock.contains_key(new_app.0) {
            client_to_start.insert(new_app.0.clone(), new_app.1.clone());
        }
    }

    drop(client_application_read_lock);

    // Starting the applications.
    // TODO if system apps are started here, they more than likly failed with systemd
    // TODO Send a email or notification to check on this system if apps are running like this

    for id in client_to_start {
        // spawn_single_application(Application::System(id.1), &mut state, state_path).await?;
        // instead of spawning let's just try to reclaim the pid

        let app_state = id.clone().1.config;

        match reclaim_child(app_state.get_pid()).await {
            Ok(mut process) => {
                if !process.monitoring() {
                    process.monitor_usage().await;
                }
                // Updating the status array
                let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
                    '_,
                    HashMap<Stringy, AppStatus>,
                > = APP_STATUS_ARRAY.try_write().await?;

                if let Some(app) = app_status_array_write_lock.get_mut(&id.0) {
                    app.app_data.set_pid(process.get_pid() as u32);
                    app.app_data.set_status(app_state.get_status());
                }

                // Adding to handler
                client_handler_write_lock
                    .insert(id.clone().1.name, SupervisedProcesses::Process(process));
                log!(
                    LogLevel::Info,
                    "{} Started and added to the client handler",
                    id.0
                );
            }
            Err(err) => {
                if err.err_type == Errors::SupervisedChild {
                    log!(LogLevel::Trace, "{} not currently running", id.0);
                    continue;
                } else {
                    return Err(err);
                }
            }
        }
    }

    Ok(())
}

pub async fn update_client_state(sys_state: &mut AppState) -> Result<(), ErrorArrayItem> {
    // Updating state files for system applications
    resolve_client_applications(&sys_state.clone().config).await?;

    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let client_application_array_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<Stringy, crate::applications::resolve::ClientApplication>,
    > = CLIENT_APPLICATION_ARRAY.try_read().await?;

    for mut_client_status in application_status_array_write_lock.iter_mut() {
        log!(
            LogLevel::Debug,
            "looking for {} in status array",
            mut_client_status.0
        );
        if let Some(new_client_state) = client_application_array_read_lock.get(&mut_client_status.0)
        {
            let state = new_client_state.config.get_state();
            mut_client_status.1.app_data.update_state(state.clone());
            if !is_pid_active(state.pid as i32).map_err(ErrorArrayItem::from)? {
                mut_client_status.1.app_data.clear_errors();
                mut_client_status.1.app_data.set_status(Status::Stopped);
            }

            calculate_uptime(mut_client_status.1, &state);
        }
    }

    Ok(())
}

pub async fn update_system_state() -> Result<(), ErrorArrayItem> {
    // Updating state files for system applications
    resolve_system_applications().await?;

    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let system_application_array_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<Stringy, crate::applications::resolve::SystemApplication>,
    > = SYSTEM_APPLICATION_ARRAY.try_read().await?;

    for mut_system_status in application_status_array_write_lock.iter_mut() {
        if let Some(new_client_state) = system_application_array_read_lock.get(&mut_system_status.0)
        {
            let state = new_client_state.config.get_state();

            mut_system_status.1.app_data.set_status(state.status);

            if !state.error_log.is_empty() {
                mut_system_status
                    .1
                    .app_data
                    .update_error_log(state.clone().error_log, false);
            } else {
                mut_system_status.1.app_data.clear_errors();
            }

            if !is_pid_active(state.pid as i32).map_err(ErrorArrayItem::from)? {
                mut_system_status.1.app_data.clear_errors();
                mut_system_status.1.app_data.set_status(Status::Stopped);
            }

            calculate_uptime(mut_system_status.1, &state);
        }
    }

    Ok(())
}

fn calculate_uptime(app: &mut AppStatus, state: &AppState) {
    check_balances(app);
    let timedout = state.last_updated <= (current_timestamp() - 30);

    if timedout {
        if let Ok(active) = is_pid_active(app.app_data.get_pid() as i32) {
            if active {
                app.app_data.set_status(Status::Warning);
                app.app_data.state.error_log.push(ErrorArrayItem::new(
                    Errors::AppState,
                    format!("TIMMED OUT. LAST UPDATED {}", state.last_updated),
                ));
                app.uptime = Some(current_timestamp() - app.timestamp);
            } else {
                app.app_data.set_status(Status::Stopped);
                app.metrics = None;
                app.uptime = None;
            }
        } else {
            app.metrics = None;
            app.uptime = None;
        }
    }

    let running: bool = app.app_data.get_status() != Status::Unknown
        && app.app_data.get_status() != Status::Stopping
        && app.app_data.get_status() != Status::Stopped;

    if !running {
        app.uptime = None;
    }

    if running && !timedout {
        app.uptime = Some(current_timestamp() - app.timestamp);
    }
}

fn check_balances(app: &mut AppStatus) {
    // Set warning if we have errors
    if app.app_data.get_status() == Status::Stopped {
        app.timestamp = current_timestamp();
        app.uptime = None;
    }

    if app.app_data.get_status() == Status::Running && !app.app_data.no_errors() {
        app.app_data.set_status(Status::Warning);
    }

    // clearing data for unknown
    if app.app_data.get_status() == Status::Unknown || app.app_data.get_status() == Status::Stopped
    {
        app.metrics = None;
        app.app_data.clear_errors();
        app.timestamp = current_timestamp();
    }

    if app.app_data.get_status() == Status::Stopping {
        app.app_data.set_status(Status::Stopped)
    }
}
