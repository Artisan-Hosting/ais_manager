use artisan_middleware::aggregator::{AppStatus, Status};
use artisan_middleware::dusa_collection_utils::functions::current_timestamp;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::{
    errors::ErrorArrayItem, log::LogLevel, rwarc::LockWithTimeout,
};
use std::collections::HashMap;

use crate::applications::child::{CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER};

use super::child::{SupervisedProcesses, APP_STATUS_ARRAY};

pub async fn monitor_applications(
    handler: LockWithTimeout<HashMap<String, SupervisedProcesses>>,
) -> Result<(), ErrorArrayItem> {
    let application_handler_read_lock = handler.try_read().await?;

    for (_, app) in application_handler_read_lock.iter() {
        match app {
            SupervisedProcesses::Child(supervised_child) => {
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

pub async fn update_application_usage() -> Result<(), ErrorArrayItem> {
    let mut app_status_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, artisan_middleware::aggregator::AppStatus>,
    > = APP_STATUS_ARRAY.try_write().await?;

    let client_application_handler = CLIENT_APPLICATION_HANDLER.clone();
    let system_application_handler = SYSTEM_APPLICATION_HANDLER.clone();

    for app in app_status_array_write_lock.iter_mut() {
        // simple house keeping tasks
        calculate_uptime(app.1);
        update_self(app.1);
        check_balances(app.1);

        match app.1.system_application {
            true => {
                let system_application_handler_read_lock =
                    system_application_handler.try_read().await?;
                if let Some(system_app) =
                    system_application_handler_read_lock.get(&app.0.to_string())
                {
                    match system_app {
                        SupervisedProcesses::Child(supervised_child) => {
                            if !supervised_child.running().await {
                                app.1.status = Status::Stopped;
                                app.1.metrics = None;
                                app.1.uptime = None;
                                continue;
                            };
                            app.1.metrics = Some(supervised_child.get_metrics().await?)
                        }
                        SupervisedProcesses::Process(supervised_process) => {
                            if !supervised_process.active() {
                                app.1.status = Status::Stopped;
                                app.1.metrics = None;
                                app.1.uptime = None;
                                continue;
                            }
                            app.1.metrics = Some(supervised_process.get_metrics().await?)
                        }
                    }
                }
            }
            false => {
                let client_application_handler_read_lock =
                    client_application_handler.try_read().await?;
                if let Some(client_app) =
                    client_application_handler_read_lock.get(&app.0.to_string())
                {
                    match client_app {
                        SupervisedProcesses::Child(supervised_child) => {
                            if !supervised_child.running().await {
                                app.1.status = Status::Stopped;
                                app.1.metrics = None;
                                app.1.uptime = None;
                                continue;
                            }
                            app.1.metrics = Some(supervised_child.get_metrics().await?)
                        }
                        SupervisedProcesses::Process(supervised_process) => {
                            if !supervised_process.active() {
                                app.1.status = Status::Stopped;
                                app.1.metrics = None;
                                app.1.uptime = None;
                                continue;
                            }
                            app.1.metrics = Some(supervised_process.get_metrics().await?)
                        }
                    }
                }
            }
        }
    }

    return Ok(());
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
        app.metrics = None;
    }
}

fn check_balances(app: &mut AppStatus) {
    // Set to running if we are recieving metric data
    if let Some(_) = app.metrics {
        app.status = Status::Running;
    }

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
