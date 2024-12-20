use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use artisan_middleware::aggregator::{save_registered_apps, AppStatus, Status};
use artisan_middleware::control::ToggleControl;
use artisan_middleware::state_persistence::StatePersistence;
use artisan_middleware::timestamp::current_timestamp;
use dusa_collection_utils::errors::{ErrorArrayItem, Errors};
use dusa_collection_utils::log;
use dusa_collection_utils::log::LogLevel;
use dusa_collection_utils::types::PathType;
use glob::glob;
use tokio::time::sleep;
use crate::{AppStatusStore, AppUpdateTimeStore, UPDATE_THRESHOLD};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub async fn process_app_status(
    app_status_store: AppStatusStore,
    execution: Arc<ToggleControl>,
    app_update_time_store: AppUpdateTimeStore,
) -> Result<(), ErrorArrayItem> {
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    // Logic for periodic status checks
    async fn check_status_as_expected(
        app_status_store: AppStatusStore,
        app_update_time_store: AppUpdateTimeStore,
    ) -> Result<(), ErrorArrayItem> {
        log!(LogLevel::Info, "Calculating status");

        let mut app_status_store_write_lock = app_status_store
            .try_write_with_timeout(Some(Duration::from_secs(2)))
            .await?;

        let app_update_time_store_write_lock = app_update_time_store
            .try_read_with_timeout(Some(Duration::from_secs(2)))
            .await?;

        for (id, app) in app_status_store_write_lock.iter_mut() {
            let last_updated = app_update_time_store_write_lock.get(id).unwrap();

            if now() - last_updated > UPDATE_THRESHOLD {
                if app.status != app.expected_status {
                    // setting the status to warning
                    if app.status != Status::Unknown {
                        app.status = Status::Warning;
                    }

                    log!(
                        LogLevel::Warn,
                        "The app: {} isn't in its expected state. Read: {}, Expected: {}",
                        id,
                        app.status,
                        app.expected_status
                    );
                }
            }
        }
        Ok(())
    }

    // Logic for marking apps idle based on timing
    async fn mark_timedout_apps(
        app_status_store: AppStatusStore,
        app_update_time_store: AppUpdateTimeStore,
    ) -> Result<(), ErrorArrayItem> {
        log!(LogLevel::Info, "Calculating idle or running");

        let mut app_status_store_write_lock = app_status_store
            .try_write_with_timeout(Some(Duration::from_secs(8)))
            .await?;

        let app_update_time_store_write_lock = app_update_time_store
            .try_read_with_timeout(Some(Duration::from_secs(8)))
            .await?;

        for app in app_status_store_write_lock.values_mut() {
            let last_update_time: &u64 = app_update_time_store_write_lock.get(&app.app_id).unwrap();
            match &app.error {
                Some(arr) => if arr.len() == 0 {
                    app.error = None
                },
                None => app.error = None,
            }

            if now() - last_update_time > UPDATE_THRESHOLD {
                match app.status {
                    Status::Running => {
                        app.status = Status::Warning;
                    }
                    _ => {
                        app.status = Status::Unknown;
                        app.uptime = Some(0);

                        let loaded_state = match StatePersistence::load_state(&PathType::Content(format!("/tmp/.{}.state", app.app_id))).await {
                            Ok(data) => data,
                            Err(err) => {
                                log!(LogLevel::Error, "Failed to load state: {}", err);
                                return Ok(())
                            }
                        };

                        app.error = Some(loaded_state.error_log);
                    }
                }
            }
        }
        Ok(())
    }

    async fn load_state_from_filesystem(
        app_status_store: AppStatusStore,
        app_update_time_store: AppUpdateTimeStore,
    ) -> Result<(), ErrorArrayItem> {
        log!(LogLevel::Info, "Loading state files from fs");
        let mut store_lock = match app_status_store.try_write().await {
            Ok(data) => data,
            Err(err) => {
                log!(LogLevel::Warn, "{}. Skipping Timeout Calculation", err);
                return Ok(());
            }
        };

        let mut time_lock = match app_update_time_store.try_write().await {
            Ok(data) => data,
            Err(err) => {
                log!(LogLevel::Warn, "{}. Skipping Timeout Calculation", err);
                return Ok(());
            }
        };

        let pattern: String = "/tmp/.*.state".to_owned();
        for file in glob(&pattern)
            .map_err(|err| ErrorArrayItem::new(Errors::GeneralError, err.msg.to_string()))?
        {
            if store_lock.len() >= 600 {
                return Ok(());
            }

            match file {
                Ok(file) => {
                    let state_path = PathType::from(file);
                    let loaded_state = match StatePersistence::load_state(&state_path).await {
                        Ok(data) => data,
                        Err(err) => {
                            log!(LogLevel::Error, "Failed to load state: {}", err);
                            continue;
                        }
                    };

                    let status = AppStatus {
                        app_id: loaded_state.clone().config.app_name,
                        status: Status::Unknown, // The the other functions will calculate it's true status
                        uptime: Some(0),
                        error: Some(loaded_state.error_log),
                        metrics: None,
                        timestamp: loaded_state.last_updated,
                        expected_status: Status::Running,
                        system_application: loaded_state.system_application,
                    };

                    log!(LogLevel::Info, "{}", status);

                    if !store_lock.contains_key(&status.app_id) {
                        let app_id = &status.app_id.clone();
                        match store_lock.insert(app_id.to_owned(), status.clone()) {
                            Some(d) => log!(LogLevel::Warn, "Status overwritten for: {}", d.app_id),
                            None => log!(LogLevel::Info, "{} added from fs", status.app_id),
                        }
                        time_lock.insert(app_id.to_owned(), current_timestamp() - (UPDATE_THRESHOLD * 2));
                    }
                }
                Err(err) => return Err(ErrorArrayItem::new(Errors::GeneralError, err.to_string())),
            }
        }

        Ok(())
    }

    async fn save_loaded_apps(app_status_store: AppStatusStore) -> Result<(), ErrorArrayItem> {
        let store = app_status_store.try_read().await?;

        let mut apps: Vec<AppStatus> = Vec::new();

        for item in store.iter() {
            apps.push(item.1.to_owned());
        }

        save_registered_apps(&apps).await
    }

    async fn update_self(app_update_time_store: AppUpdateTimeStore) -> Result<(), ErrorArrayItem> {
        let mut app_time = app_update_time_store.try_write().await?;
        app_time.insert("Aggregator".into(), current_timestamp());
        drop(app_time);
        Ok(())
    }

    // Consolidated workflow
    loop {
        execution.wait_if_paused().await;

        log!(LogLevel::Info, "Starting periodic operations");

        update_self(app_update_time_store.clone()).await?;
        sleep(Duration::from_millis(100)).await;
        save_loaded_apps(app_status_store.clone()).await?;
        sleep(Duration::from_millis(100)).await;
        check_status_as_expected(app_status_store.clone(), app_update_time_store.clone()).await?;
        sleep(Duration::from_millis(100)).await;
        mark_timedout_apps(app_status_store.clone(), app_update_time_store.clone()).await?;
        load_state_from_filesystem(app_status_store.clone(), app_update_time_store.clone()).await?;
        sleep(Duration::from_millis(100)).await;
        trim(app_status_store.clone()).await?;

        // Optional sleep for throttling, if necessary
        sleep(Duration::from_secs(10)).await;
    }
}

pub async fn trim(app_status_store: AppStatusStore) -> Result<(), ErrorArrayItem> {
    let mut store = match app_status_store.try_write().await {
        Ok(data) => data,
        Err(_) => return Ok(()),
    };

    if store.len() >= 600 {
        let excess = store.len() - 600;
        println!("{}", excess);
        println!("{}", store.len());
        let keys_to_remove: Vec<_> = store.keys().take(excess).cloned().collect();

        for key in keys_to_remove {
            log!(
                LogLevel::Warn,
                "Internal store over 600. Dropping: {}",
                &key
            );
            store.remove(&key);
        }
    }

    Ok(())
}

// Logic for updating app uptime
pub async fn update_uptime(
    app_status_store: AppStatusStore,
    app_update_time_store: AppUpdateTimeStore,
) -> Result<(), ErrorArrayItem> {
    log!(LogLevel::Info, "Calculating uptime");
    let mut app_status_store_write_lock = app_status_store.try_write_with_timeout(Some(Duration::from_secs(2))).await?;

    let app_update_time_store_write_lock = app_update_time_store.try_read().await?;

    for (id, app) in app_status_store_write_lock.iter_mut() {
        log!(LogLevel::Trace, "Calculating uptime for: {}", id);
        let last_update_time = app_update_time_store_write_lock.get(id).unwrap();

        if app.status == Status::Running
            || app.status == Status::Warning
            || app.status == Status::Starting
            || app.status == Status::Stopping
        {
            if now() - last_update_time < UPDATE_THRESHOLD {
                app.uptime = Some(now() - app.timestamp);
            }
        }
    }

    drop(app_status_store_write_lock);
    Ok(())
}
