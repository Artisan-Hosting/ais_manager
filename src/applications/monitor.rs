use artisan_middleware::aggregator::{AppStatus, Metrics, Status};
use artisan_middleware::dusa_collection_utils::core::errors::Errors;
use artisan_middleware::dusa_collection_utils::core::functions::current_timestamp;
use artisan_middleware::dusa_collection_utils::core::types::rwarc::LockWithTimeout;
use artisan_middleware::dusa_collection_utils::core::types::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::{
    core::errors::ErrorArrayItem, core::logger::LogLevel,
};
use artisan_middleware::process_manager::is_pid_active;
use artisan_middleware::resource_monitor::ResourceMonitorLock;
use artisan_middleware::state_persistence::AppState;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::applications::child::{
    CLIENT_APPLICATION_ARRAY, CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER,
};
use crate::applications::resolve::{
    resolve_client_applications, resolve_system_applications, SystemApplication,
};
use crate::system::control::GlobalState;
use crate::system::ebpf::{debug_print_aggregated, TrafficStats};

use super::child::{SupervisedProcesses, APP_STATUS_ARRAY, SYSTEM_APPLICATION_ARRAY};
use super::pid::reclaim_child;
use super::resolve::ClientApplication;

pub async fn monitor_application_resource_usage(
    handler: LockWithTimeout<HashMap<Stringy, SupervisedProcesses>>,
    gs: &Arc<GlobalState>,
) -> Result<(), ErrorArrayItem> {
    let application_handler_read_lock = handler.try_read().await?;

    // Define an inner asynchronous function (note: use fn, not closure)
    async fn update_usage(
        name: &Stringy,
        pid: u32,
        monitor: &ResourceMonitorLock,
        app_status_array_write_lock: &mut HashMap<Stringy, AppStatus>,
        gs: &Arc<GlobalState>,
    ) -> Result<(), ErrorArrayItem> {
        match monitor.0.try_write_with_timeout(None).await {
            Ok(mut monitor_lock) => {
                let usage = monitor_lock.aggregate_tree_usage()?;
                log!(
                    LogLevel::Debug,
                    "{} usage : cpu:{}, ram: {}",
                    pid,
                    usage.0,
                    usage.1
                );
                monitor_lock.cpu = usage.0;
                monitor_lock.ram = usage.1;

                let net_usage: HashMap<String, TrafficStats> =
                    gs.network_monitor.aggregate_bandwidth_by_service().await?;
                let service_network: Option<artisan_middleware::aggregator::NetworkUsage> =
                    if let Some(net) = net_usage.get(&name.to_string()) {
                        Some(net.to_network_usage())
                    } else {
                        None
                    };

                // update ledger
                let current: Metrics = Metrics {
                    cpu_usage: monitor_lock.cpu,
                    memory_usage: monitor_lock.ram,
                    other: service_network,
                };

                gs.ledger
                    .try_write()
                    .await?
                    .update_application_usage(name.clone(), current.clone());

                debug_print_aggregated(net_usage);

                if let Some(app_status) = app_status_array_write_lock.get_mut(name) {
                    app_status.metrics = Some(current);
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    for (name, app) in application_handler_read_lock.iter() {
        log!(LogLevel::Debug, "USAGE MONITOR: -> {}", name);
        match app {
            SupervisedProcesses::Child(child) => {
                let mut app_status_array_write_lock = APP_STATUS_ARRAY.try_write().await?;

                if !child.running().await {
                    continue;
                }
                let pid = child.get_pid().await?;
                if let Err(err) = update_usage(
                    name,
                    pid,
                    &child.monitor,
                    &mut app_status_array_write_lock,
                    &gs.clone(),
                )
                .await
                {
                    drop(app_status_array_write_lock);
                    log!(LogLevel::Error, "Error locking monitor: {}", err);
                    break;
                }
                drop(app_status_array_write_lock);
            }
            SupervisedProcesses::Process(process) => {
                let mut app_status_array_write_lock = APP_STATUS_ARRAY.try_write().await?;

                if !process.active() {
                    continue;
                }
                let pid = process.get_pid() as u32;
                if let Err(err) = update_usage(
                    name,
                    pid,
                    &process.monitor,
                    &mut app_status_array_write_lock,
                    &gs.clone(),
                )
                .await
                {
                    drop(app_status_array_write_lock);
                    log!(LogLevel::Error, "Error locking monitor: {}", err);
                    break;
                }
                drop(app_status_array_write_lock);
            }
        };
    }

    Ok(())
}

pub async fn handle_dead_applications() -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Debug, "Handling dead applications");

    let mut system_handler_write_lock = SYSTEM_APPLICATION_HANDLER
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;
    let mut client_handler_write_lock = CLIENT_APPLICATION_HANDLER
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;

    let mut system_handler_to_remove = HashSet::new();
    let mut client_handler_to_remove = HashSet::new();

    // Closure to process handlers
    async fn process_handlers(
        handler: &mut HashMap<Stringy, SupervisedProcesses>,
        app_statuses: &mut std::collections::HashMap<
            Stringy,
            artisan_middleware::aggregator::AppStatus,
        >,
        to_remove: &mut HashSet<Stringy>,
    ) {
        for (app_name, process) in handler.iter_mut() {
            if let Some(app_status) = app_statuses.get_mut(app_name) {
                let should_remove = match process {
                    SupervisedProcesses::Child(child) => {
                        let running = child.running().await;
                        if !running {
                            child.terminate_monitor();
                        }
                        !running
                    }
                    SupervisedProcesses::Process(proc) => {
                        let active = proc.active();
                        if !active {
                            proc.terminate_monitor();
                        }
                        !active
                    }
                };

                if should_remove {
                    app_status.app_data.set_status(Status::Stopped);
                    app_status.metrics = None;
                    app_status.uptime = None;
                    app_status.timestamp = current_timestamp();

                    to_remove.insert(app_status.app_data.get_name().into());
                }
            }
        }
    }

    let mut app_status_array_write_lock = APP_STATUS_ARRAY
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;

    // Process system and client handlers
    process_handlers(
        &mut system_handler_write_lock,
        &mut app_status_array_write_lock,
        &mut system_handler_to_remove,
    )
    .await;

    process_handlers(
        &mut client_handler_write_lock,
        &mut app_status_array_write_lock,
        &mut client_handler_to_remove,
    )
    .await;

    drop(app_status_array_write_lock);

    // Generic removal function
    fn remove_dead_apps(
        handler: &mut HashMap<Stringy, SupervisedProcesses>,
        to_remove: &HashSet<Stringy>,
        handler_name: &str,
    ) {
        for id in to_remove {
            if handler.remove(id).is_some() {
                log!(
                    LogLevel::Info,
                    "Removed: {} from {} handler",
                    id,
                    handler_name
                );
            }
        }
    }

    remove_dead_apps(
        &mut system_handler_write_lock,
        &system_handler_to_remove,
        "system",
    );

    remove_dead_apps(
        &mut client_handler_write_lock,
        &client_handler_to_remove,
        "client",
    );

    Ok(())
}

pub async fn handle_new_system_applications(gs: &Arc<GlobalState>) -> Result<(), ErrorArrayItem> {
    // resolve current applications
    resolve_system_applications(gs).await?;

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

                drop(app_status_array_write_lock);

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

pub async fn handle_new_client_applications(gs: &Arc<GlobalState>) -> Result<(), ErrorArrayItem> {
    // resolve current applications
    resolve_client_applications(&gs.clone()).await?;

    let mut client_handler_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, SupervisedProcesses>,
    > = CLIENT_APPLICATION_HANDLER
        .try_write()
        .await
        .map_err(|mut err| {
            err.err_mesg = format!(
                "Error getting write lock on client handler: {}",
                err.err_mesg
            )
            .into();
            err
        })?;

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
                > = APP_STATUS_ARRAY.try_write().await.map_err(|mut err| {
                    err.err_mesg = format!(
                        "Error getting write lock on reclaiming child app status array: {}",
                        err.err_mesg
                    )
                    .into();
                    err
                })?;

                if let Some(app) = app_status_array_write_lock.get_mut(&id.0) {
                    app.app_data.set_pid(process.get_pid() as u32);
                    app.app_data.set_status(app_state.get_status());
                }

                drop(app_status_array_write_lock);

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

pub async fn update_client_state(gs: &Arc<GlobalState>) -> Result<(), ErrorArrayItem> {
    // Updating state files for system applications

    resolve_client_applications(&gs.clone()).await?;

    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await.map_err(|mut err| {
        err.err_mesg = format!(
            "Error getting write lock on app status in update client state: {}",
            err.err_mesg
        )
        .into();
        err
    })?;

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
            } else {
                mut_client_status.1.app_data.state.error_log.truncate(5);

                if !mut_client_status.1.app_data.state.stdout.is_empty() {
                    mut_client_status.1.app_data.state.stdout.reverse();
                    mut_client_status.1.app_data.state.stdout.truncate(500);
                    mut_client_status.1.app_data.state.stdout.reverse();
                }
                if !mut_client_status.1.app_data.state.stderr.is_empty() {
                    mut_client_status.1.app_data.state.stderr.reverse();
                    mut_client_status.1.app_data.state.stderr.truncate(500);
                    mut_client_status.1.app_data.state.stderr.reverse();
                }
            }

            calculate_uptime(mut_client_status.1, &state);
        }
    }

    drop(application_status_array_write_lock);

    Ok(())
}

pub async fn update_system_state(gs: &Arc<GlobalState>) -> Result<(), ErrorArrayItem> {
    // Updating state files for system applications
    resolve_system_applications(gs).await?;

    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await.map_err(|mut err| {
        err.err_mesg = format!(
            "Error getting write lock on app status in update system state: {}",
            err.err_mesg
        )
        .into();
        err
    })?;

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
            } else {
                mut_system_status.1.app_data.state.error_log.truncate(5);

                if !mut_system_status.1.app_data.state.stdout.is_empty() {
                    mut_system_status.1.app_data.state.stdout.reverse();
                    mut_system_status.1.app_data.state.stdout.truncate(500);
                    mut_system_status.1.app_data.state.stdout.reverse();
                }
                if !mut_system_status.1.app_data.state.stderr.is_empty() {
                    mut_system_status.1.app_data.state.stderr.reverse();
                    mut_system_status.1.app_data.state.stderr.truncate(500);
                    mut_system_status.1.app_data.state.stderr.reverse();
                }
            }

            calculate_uptime(mut_system_status.1, &state);
        }
    }

    drop(application_status_array_write_lock);

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
