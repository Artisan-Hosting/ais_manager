use artisan_middleware::aggregator::{AppStatus, Metrics, Status};
use artisan_middleware::dusa_collection_utils::errors::Errors;
use artisan_middleware::dusa_collection_utils::functions::current_timestamp;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::{
    errors::ErrorArrayItem, log::LogLevel, rwarc::LockWithTimeout,
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
    handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>,
) -> Result<(), ErrorArrayItem> {
    let application_handler_read_lock = handler.try_read().await?;

    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    for (name, app) in application_handler_read_lock.iter() {
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

                        if let Some(app_arr_val) = app_status_array_write_lock.get_mut(&name.into())
                        {
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

                        if let Some(app_arr_val) = app_status_array_write_lock.get_mut(&name.into())
                        {
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
    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let mut system_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<String, SupervisedProcesses>,
    > = SYSTEM_APPLICATION_HANDLER.try_write().await?;

    let mut system_handler_to_remove: HashSet<Stringy> = HashSet::new();

    let mut client_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<String, SupervisedProcesses>,
    > = CLIENT_APPLICATION_HANDLER.try_write().await?;

    let mut client_handler_to_remove: HashSet<Stringy> = HashSet::new();

    for system_handle in system_handler_write_lock.iter_mut() {
        let system_name: &String = system_handle.0;
        if let Some(system_application_status) =
            app_status_array_write_lock.get_mut(&system_name.into())
        {
            match system_handle.1 {
                SupervisedProcesses::Child(supervised_child) => {
                    if !supervised_child.running().await {
                        // Set status properly
                        system_application_status.state.status = Status::Stopped;
                        system_application_status.metrics = None;
                        system_application_status.uptime = None;
                        system_application_status.timestamp = current_timestamp();

                        // put in to be removed set
                        system_handler_to_remove.insert(system_application_status.app_id.clone());

                        // Dropping the monitor
                        supervised_child.terminate_monitor();
                    }
                }
                SupervisedProcesses::Process(supervised_process) => {
                    if !supervised_process.active() {
                        // Set status properly
                        system_application_status.state.status = Status::Stopped;
                        system_application_status.metrics = None;
                        system_application_status.uptime = None;
                        system_application_status.timestamp = current_timestamp();

                        // put in to be removed set
                        system_handler_to_remove.insert(system_application_status.app_id.clone());

                        // Dropping the monitor
                        supervised_process.terminate_monitor();
                    }
                }
            }
        }
    }

    for client_handle in client_handler_write_lock.iter_mut() {
        let client_name: &String = client_handle.0;
        if let Some(client_application_status) =
            app_status_array_write_lock.get_mut(&client_name.into())
        {
            match client_handle.1 {
                SupervisedProcesses::Child(supervised_child) => {
                    if !supervised_child.running().await {
                        // Set status properly
                        client_application_status.state.status = Status::Stopped;
                        client_application_status.metrics = None;
                        client_application_status.uptime = None;
                        client_application_status.timestamp = current_timestamp();


                        // put in to be removed set
                        client_handler_to_remove.insert(client_application_status.app_id.clone());

                        // Dropping the monitor
                        supervised_child.terminate_monitor();
                    }
                }
                SupervisedProcesses::Process(supervised_process) => {
                    if !supervised_process.active() {
                        // Set status properly
                        client_application_status.state.status = Status::Stopped;
                        client_application_status.metrics = None;
                        client_application_status.uptime = None;
                        client_application_status.timestamp = current_timestamp();

                        // put in to be removed set
                        client_handler_to_remove.insert(client_application_status.app_id.clone());

                        // Dropping the monitor
                        supervised_process.terminate_monitor();
                    }
                }
            }
        }
    }

    // removing system apps
    for id in system_handler_to_remove.iter() {
        if let Some(_) = system_handler_write_lock.remove(&id.to_string()) {
            log!(LogLevel::Info, "Removed: {} from system handler", id);
        }
    }

    // removing client apps
    for id in client_handler_to_remove.iter() {
        if let Some(_) = client_handler_write_lock.remove(&id.to_string()) {
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
        HashMap<String, SupervisedProcesses>,
    > = SYSTEM_APPLICATION_HANDLER.try_write().await?;

    let system_application_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, crate::applications::resolve::SystemApplication>,
    > = SYSTEM_APPLICATION_ARRAY.try_read().await?;

    let mut system_to_start: HashMap<Stringy, SystemApplication> = HashMap::new();

    for new_app in system_application_read_lock.iter() {
        if !system_handler_write_lock.contains_key(new_app.0) {
            system_to_start.insert(new_app.0.into(), new_app.1.clone());
        }
    }

    drop(system_application_read_lock);

    // Starting the applications.
    // TODO if system apps are started here, they more than likly failed with systemd
    // TODO Send a email or notification to check on this system if apps are running like this

    for id in system_to_start {
        // spawn_single_application(Application::System(id.1), &mut state, state_path).await?;
        // instead of spawning let's just try to reclaim the pid

        if let Some(app_state) = id.1.state {
            match reclaim_child(app_state.pid).await {
                Ok(mut process) => {
                    process.monitor_usage().await;

                    // Updating the status array
                    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
                        '_,
                        HashMap<Stringy, AppStatus>,
                    > = APP_STATUS_ARRAY.try_write().await?;

                    if let Some(app) = app_status_array_write_lock.get_mut(&id.0) {
                        app.state.pid = process.get_pid() as u32;
                        app.state.status = app_state.status;
                        if app.state.status == Status::Idle {
                            app.metrics = None;
                        }
                    }

                    // Adding to handler
                    system_handler_write_lock
                        .insert(id.0.to_string(), SupervisedProcesses::Process(process));
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
    }

    Ok(())
}

pub async fn handle_new_client_applications(state: &mut AppState) -> Result<(), ErrorArrayItem> {
    // resolve current applications
    resolve_client_applications(&state.config).await?;

    let mut client_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<String, SupervisedProcesses>,
    > = CLIENT_APPLICATION_HANDLER.try_write().await?;

    let client_application_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, crate::applications::resolve::ClientApplication>,
    > = CLIENT_APPLICATION_ARRAY.try_read().await?;

    let mut client_to_start: HashMap<Stringy, ClientApplication> = HashMap::new();

    for new_app in client_application_read_lock.iter() {
        if !client_handler_write_lock.contains_key(new_app.0) {
            client_to_start.insert(new_app.0.into(), new_app.1.clone());
        }
    }

    drop(client_application_read_lock);

    // Starting the applications.
    // TODO if system apps are started here, they more than likly failed with systemd
    // TODO Send a email or notification to check on this system if apps are running like this

    for id in client_to_start {
        // spawn_single_application(Application::System(id.1), &mut state, state_path).await?;
        // instead of spawning let's just try to reclaim the pid

        if let Some(app_state) = id.1.state {
            match reclaim_child(app_state.pid).await {
                Ok(mut process) => {
                    process.monitor_usage().await;

                    // Updating the status array
                    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
                        '_,
                        HashMap<Stringy, AppStatus>,
                    > = APP_STATUS_ARRAY.try_write().await?;

                    if let Some(app) = app_status_array_write_lock.get_mut(&id.0) {
                        app.state.pid = process.get_pid() as u32;
                        app.state.status = app_state.status;
                    }

                    // Adding to handler
                    client_handler_write_lock
                        .insert(id.0.to_string(), SupervisedProcesses::Process(process));
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
    }

    Ok(())
}

pub async fn update_client_state(
    state: &mut AppState,
) -> Result<(), ErrorArrayItem> {
    // Updating state files for system applications
    resolve_client_applications(&state.clone().config).await?;

    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let client_application_array_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, crate::applications::resolve::ClientApplication>,
    > = CLIENT_APPLICATION_ARRAY.try_read().await?;

    for mut_client_status in application_status_array_write_lock.iter_mut() {
        if let Some(new_client_state) =
            client_application_array_read_lock.get(&mut_client_status.0.to_string())
        {
            if let Some(state) = &new_client_state.state {
                mut_client_status.1.state = state.clone();
                if !is_pid_active(state.pid as i32).map_err(ErrorArrayItem::from)? {
                    mut_client_status.1.state.error_log.clear();
                    mut_client_status.1.state.status = Status::Stopped;
                }

                calculate_uptime(mut_client_status.1, state);
            }
        }
    }

    Ok(())
}

pub async fn update_system_state(
) -> Result<(), ErrorArrayItem> {
    // Updating state files for system applications
    resolve_system_applications().await?;

    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let system_application_array_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, crate::applications::resolve::SystemApplication>,
    > = SYSTEM_APPLICATION_ARRAY.try_read().await?;

    for mut_system_status in application_status_array_write_lock.iter_mut() {
        
        if let Some(new_client_state) =
            system_application_array_read_lock.get(&mut_system_status.0.to_string())
        {
            if let Some(state) = &new_client_state.state {
                mut_system_status.1.state.status = state.status;
                if !state.error_log.is_empty() {
                    mut_system_status.1.state.error_log = state.error_log.clone();
                } else {
                    mut_system_status.1.state.error_log.clear();
                }

                if !is_pid_active(state.pid as i32).map_err(ErrorArrayItem::from)? {
                    mut_system_status.1.state.error_log.clear();
                    mut_system_status.1.state.status = Status::Stopped;
                }

                calculate_uptime(mut_system_status.1, state);
            }
        }
    }

    Ok(())
}

fn calculate_uptime(app: &mut AppStatus, state: &AppState) {
    check_balances(app);
    let timedout = state.last_updated <= (current_timestamp() - 30);

    if timedout {
        app.state.status = Status::Unknown;
        app.metrics = None;
    }

    let running: bool = app.state.status != Status::Unknown
        && app.state.status != Status::Stopping
        && app.state.status != Status::Stopped;

    if !running {
        app.uptime = None;
    }

    if running && !timedout {
        app.uptime = Some(current_timestamp() - app.timestamp);
    }
}

fn check_balances(app: &mut AppStatus) {
    // Set warning if we have errors
    if app.state.status == Status::Stopped {
        app.timestamp = current_timestamp();
        app.uptime = None;
    }

    if app.state.status == Status::Running && !app.state.error_log.is_empty() {
        app.state.status = Status::Warning;
    }

    // clearing data for unknown
    if app.state.status == Status::Unknown || app.state.status == Status::Stopped {
        app.metrics = None;
        app.state.error_log.clear();
        app.timestamp = current_timestamp();
    }

    if app.state.status == Status::Stopping {
        app.state.status = Status::Stopped
    }
}

