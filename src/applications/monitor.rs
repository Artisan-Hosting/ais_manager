use artisan_middleware::aggregator::{AppStatus, Status};
use artisan_middleware::config::AppConfig;
use artisan_middleware::dusa_collection_utils::functions::current_timestamp;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::types::PathType;
use artisan_middleware::dusa_collection_utils::{
    errors::ErrorArrayItem, log::LogLevel, rwarc::LockWithTimeout,
};
use artisan_middleware::process_manager::SupervisedProcess;
use artisan_middleware::state_persistence::AppState;
use signal_hook::iterator::Handle;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::applications::child::{
    spawn_single_application, CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER,
};
use crate::applications::resolve::{
    resolve_client_applications, resolve_system_applications, Application, SystemApplication,
};

use super::child::{
    populate_initial_state_lock, SupervisedProcesses, APP_STATUS_ARRAY, CLIENT_APPLICATION_ARRAY, SYSTEM_APPLICATION_ARRAY
};
use super::pid::reclaim_child;

pub async fn monitor_application_resource_usage(
    handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>,
) -> Result<(), ErrorArrayItem> {
    let application_handler_read_lock = handler.try_read().await?;

    for (_, app) in application_handler_read_lock.iter() {
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

pub async fn track_application_uptime() -> Result<(), ErrorArrayItem> {
    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    for app in app_status_array_write_lock.iter_mut() {
        // simple house keeping tasks
        calculate_uptime(app.1);
        update_self(app.1);
        check_balances(app.1);
    }

    return Ok(());
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
                        system_application_status.status = Status::Stopped;
                        system_application_status.metrics = None;
                        system_application_status.uptime = None;

                        // put in to be removed set
                        system_handler_to_remove.insert(system_application_status.app_id.clone());

                        // Dropping the monitor
                        supervised_child.terminate_monitor();
                    }
                }
                SupervisedProcesses::Process(supervised_process) => {
                    if !supervised_process.active() {
                        // Set status properly
                        system_application_status.status = Status::Stopped;
                        system_application_status.metrics = None;
                        system_application_status.uptime = None;

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
                        client_application_status.status = Status::Stopped;
                        client_application_status.metrics = None;
                        client_application_status.uptime = None;

                        // put in to be removed set
                        client_handler_to_remove.insert(client_application_status.app_id.clone());

                        // Dropping the monitor
                        supervised_child.terminate_monitor();
                    }
                }
                SupervisedProcesses::Process(supervised_process) => {
                    if !supervised_process.active() {
                        // Set status properly
                        client_application_status.status = Status::Stopped;
                        client_application_status.metrics = None;
                        client_application_status.uptime = None;

                        // put in to be removed set
                        system_handler_to_remove.insert(client_application_status.app_id.clone());

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

pub async fn handle_new_system_applications(
    mut state: &mut AppState,
    state_path: &PathType,
) -> Result<(), ErrorArrayItem> {
    // resolve current applications
    resolve_system_applications().await?;

    let system_handler_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, SupervisedProcesses>,
    > = SYSTEM_APPLICATION_HANDLER.try_read().await?;

    let system_application_read_lock: tokio::sync::RwLockReadGuard<
        '_,
        HashMap<String, crate::applications::resolve::SystemApplication>,
    > = SYSTEM_APPLICATION_ARRAY.try_read().await?;

    let mut system_to_start: HashMap<Stringy, SystemApplication> = HashMap::new();

    for new_app in system_application_read_lock.iter() {
        if !system_handler_read_lock.contains_key(new_app.0) {
            system_to_start.insert(new_app.0.into(), new_app.1.clone());
        }
    }

    drop(system_application_read_lock);
    drop(system_handler_read_lock);

    // Starting the applications.
    // TODO if system apps are started here, they more than likly failed with systemd
    // TODO Send a email or notification to check on this system if apps are running like this

    for id in system_to_start {
        spawn_single_application(Application::System(id.1), &mut state, state_path).await?;

        // Setting the status for the application
        populate_initial_state_lock(state).await?;

        // Ensuring the application is in the handler
        let system_handler_read_lock = SYSTEM_APPLICATION_HANDLER.try_read().await?;
        let name: &Stringy = &id.0;
        if let Some(_) = system_handler_read_lock.get(&name.to_string()) {
            log!(
                LogLevel::Info,
                "{} Started and added to the system handler",
                name
            );
        }
    }

    Ok(())
}

pub async fn _update_application_state_from_disk(
    state: AppState,
    config: &AppConfig,
) -> Result<(), ErrorArrayItem> {
    // Update both indexs
    resolve_client_applications(config).await?;
    resolve_system_applications().await?;

    let mut application_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        HashMap<Stringy, AppStatus>,
    > = APP_STATUS_ARRAY
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;

    if state.config.environment != "systemonly" {
        let client_application_array_read_lock = CLIENT_APPLICATION_ARRAY.try_read().await?;

        for client in client_application_array_read_lock.values() {
            if let Some(application_array_entry) =
                application_status_array_write_lock.get_mut(&Stringy::from(client.name.clone()))
            {
                if let Some(state) = &client.state {
                    application_array_entry.status = state.status;

                    if state.error_log.is_empty() {
                        application_array_entry.error = None;
                    } else {
                        application_array_entry.error = Some(state.error_log.clone())
                    };

                    // TODO add system to re-capture the pid if it's diffrent from what we have
                    if state.pid != application_array_entry.pid {
                        let mut client_application_handler_write_lock: tokio::sync::RwLockWriteGuard<
                            '_,
                            HashMap<String, SupervisedProcesses>,
                        > = CLIENT_APPLICATION_HANDLER.try_write().await?;

                        let mut captured_process: SupervisedProcess =
                            reclaim_child(state.pid).await?;
                        captured_process.monitor_usage().await;
                        if let Some(old_handle) = client_application_handler_write_lock.insert(
                            state.name.clone(),
                            SupervisedProcesses::Process(captured_process),
                        ) {
                            match old_handle {
                                SupervisedProcesses::Child(proc) => {
                                    log!(
                                        LogLevel::Info,
                                        "Old application handle dropped: {}",
                                        proc.get_pid().await?
                                    )
                                }
                                SupervisedProcesses::Process(proc) => {
                                    log!(
                                        LogLevel::Info,
                                        "Old application handle dropped: {}",
                                        proc.get_pid()
                                    )
                                }
                            }
                        };
                    }
                } else {
                    log!(LogLevel::Debug, "Entry: {} has no state file", client.name);
                    continue;
                }
            } else {
                log!(
                    LogLevel::Debug,
                    "Entry: {} not in the array, we might be out of data",
                    client.name
                );
                continue;
            }
        }
    }

    // System applications
    {
        let system_application_array_read_lock = SYSTEM_APPLICATION_ARRAY.try_read().await?;

        for system in system_application_array_read_lock.values() {
            if let Some(application_array_entry) =
                application_status_array_write_lock.get_mut(&Stringy::from(system.name.clone()))
            {
                if let Some(state) = &system.state {
                    application_array_entry.status = state.status;

                    if state.error_log.is_empty() {
                        application_array_entry.error = None;
                    } else {
                        application_array_entry.error = Some(state.error_log.clone())
                    };

                    // TODO add system to re-capture the pid if it's diffrent from what we have
                    if state.pid != application_array_entry.pid {
                        let mut system_application_handler_write_lock: tokio::sync::RwLockWriteGuard<
                            '_,
                            HashMap<String, SupervisedProcesses>,
                        > = CLIENT_APPLICATION_HANDLER.try_write().await?;

                        let mut captured_process: SupervisedProcess =
                            reclaim_child(state.pid).await?;
                        captured_process.monitor_usage().await;

                        system_application_handler_write_lock.remove(&state.name.clone());
                        system_application_handler_write_lock.insert(
                            state.name.clone(),
                            SupervisedProcesses::Process(captured_process),
                        );

                        // if let Some(old_handle) = system_application_handler_write_lock.insert(
                        //     state.name.clone(),
                        //     SupervisedProcesses::Process(captured_process),
                        // ) {
                        //     match old_handle {
                        //         SupervisedProcesses::Child(proc) => {
                        //             log!(LogLevel::Info, "Old application handle dropped: {}", proc.get_pid().await?)
                        //         }
                        //         SupervisedProcesses::Process(proc) => {
                        //             log!(LogLevel::Info, "Old application handle dropped: {}", proc.get_pid())
                        //         }
                        //     }
                        // };
                    }
                } else {
                    log!(LogLevel::Debug, "Entry: {} has no state file", system.name);
                    continue;
                }
            } else {
                log!(
                    LogLevel::Debug,
                    "Entry: {} not in the array, we might be out of data",
                    system.name
                );
                continue;
            }
        }
    }

    Ok(())
}

fn calculate_uptime(app: &mut AppStatus) {
    let is_running: bool = app.status != Status::Unknown
        && app.status != Status::Stopping
        && app.status != Status::Stopped;

    if is_running {
        app.uptime = Some(current_timestamp() - app.timestamp);
    } else {
        app.uptime = None;
        app.metrics = None;
        app.timestamp = current_timestamp();
    }
}

fn update_self(app: &mut AppStatus) {
    if app.app_id == Stringy::from("ais_manager") {
        app.status = Status::Idle;
        // app.metrics = None;
    }
}

fn check_balances(app: &mut AppStatus) {
    // Set to running if we are recieving metric data
    // if let Some(_) = app.metrics {
    // app.status = Status::Running;
    // }

    // Set warning if we have errors
    if app.status == Status::Running && app.error.is_some() {
        app.status = Status::Warning;
    }

    // ? If an app is in the idle state, we just monitor it's uptime, we don't track errors or metrics
    if app.status == Status::Idle {
        app.metrics = None;
        app.error = None;
    }

    // clearing data for unknown
    if app.status == Status::Unknown {
        app.metrics = None;
        app.error = None;
        app.timestamp = current_timestamp();
    }
}

// pub async fn monitor_client_applications(client_handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>) -> Result<(), ErrorArrayItem> {
//     let client_application_handler_read_lock
//     = client_handler.try_read().await?;

//     for (key, client_app) in client_application_handler_read_lock.iter() {
//         let key = key.clone(); // Clone the key (or data needed for logging) into the closure
//         let client_app = client_app.clone(); // Clone client_app if it's clonable or move if it owns its data

//         tokio::spawn(async move {
//             log!(LogLevel::Info, "monitoring: {}", key);
//             let resource: ResourceMonitorLock = client_app.monitor;
//             resource.monitor(2).await;
//         });
//     }

//     Ok(())
// }
