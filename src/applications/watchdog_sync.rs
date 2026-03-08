use artisan_middleware::{
    aggregator::{Metrics, NetworkUsage, Status},
    dusa_collection_utils::core::{
        errors::{ErrorArrayItem, Errors},
        functions::current_timestamp,
        logger::LogLevel,
        types::{pathtype::PathType, stringy::Stringy},
    },
    dusa_collection_utils::log,
    state_persistence::{AppState, StatePersistence},
};

use super::child::{make_app_status, APP_STATUS_ARRAY};

fn map_watchdog_status(status: &str) -> Status {
    match status.trim().to_ascii_lowercase().as_str() {
        "starting" => Status::Starting,
        "running" => Status::Running,
        "idle" => Status::Idle,
        "stopping" => Status::Stopping,
        "stopped" => Status::Stopped,
        "warning" => Status::Warning,
        "building" => Status::Building,
        _ => Status::Unknown,
    }
}

fn metrics_from_watchdog(
    status: &crate::watchdog::proto::ApplicationStatusMessage,
) -> Option<Metrics> {
    let mapped = map_watchdog_status(&status.status);
    if matches!(
        mapped,
        Status::Stopped | Status::Unknown | Status::Stopping | Status::Starting | Status::Building
    ) {
        return None;
    }

    let other = status.network_usage.as_ref().map(|network| NetworkUsage {
        rx_bytes: network.rx_bytes,
        tx_bytes: network.tx_bytes,
    });

    Some(Metrics {
        cpu_usage: status.cpu_usage,
        memory_usage: status.memory_usage,
        other,
    })
}

fn state_paths_for_app(app_name: &str) -> [PathType; 2] {
    [
        PathType::Content(format!("/tmp/.{}.state", app_name)),
        PathType::Content(format!("/opt/artisan/tmp/.{}.state", app_name)),
    ]
}

async fn load_app_state(app_name: &str) -> Option<AppState> {
    for path in state_paths_for_app(app_name) {
        match StatePersistence::load_state(&path).await {
            Ok(state) => return Some(state),
            Err(_) => continue,
        }
    }
    None
}

fn apply_watchdog_status(
    app_status: &mut artisan_middleware::aggregator::AppStatus,
    status: &crate::watchdog::proto::ApplicationStatusMessage,
    now: u64,
) {
    app_status.app_data.set_status(map_watchdog_status(&status.status));
    if let Some(pid) = status.pid {
        app_status.app_data.set_pid(pid);
    }
    app_status.metrics = metrics_from_watchdog(status);
    app_status.timestamp = status.last_updated.max(now);
}

pub async fn refresh_status_from_watchdog() -> Result<(), ErrorArrayItem> {
    let statuses = crate::watchdog::list_applications().await?;
    let now = current_timestamp();

    let existing_keys: std::collections::HashSet<Stringy> = {
        let status_array = APP_STATUS_ARRAY
            .try_read()
            .await
            .map_err(|err| ErrorArrayItem::new(Errors::TimedOut, err.to_string()))?;
        status_array.keys().cloned().collect()
    };

    let mut inserts: Vec<(Stringy, artisan_middleware::aggregator::AppStatus)> = Vec::new();
    for status in &statuses {
        let key: Stringy = status.name.clone().into();
        if existing_keys.contains(&key) {
            continue;
        }

        let Some(state) = load_app_state(&status.name).await else {
            log!(
                LogLevel::Warn,
                "Watchdog reported app '{}' but no state file found; skipping inventory insert",
                status.name
            );
            continue;
        };

        let mut app_config =
            artisan_middleware::config_bundle::ApplicationConfig::new(state, None, None);
        app_config.set_status(map_watchdog_status(&status.status));
        if let Some(pid) = status.pid {
            app_config.set_pid(pid);
        }

        let Ok((key, mut new_status)) = make_app_status(&status.name, app_config) else {
            continue;
        };
        apply_watchdog_status(&mut new_status, status, now);
        inserts.push((key, new_status));
    }

    let mut status_array = APP_STATUS_ARRAY
        .try_write()
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::TimedOut, err.to_string()))?;

    for status in statuses {
        let key: Stringy = status.name.clone().into();
        if let Some(app_status) = status_array.get_mut(&key) {
            apply_watchdog_status(app_status, &status, now);
        }
    }

    for (key, app_status) in inserts {
        status_array.insert(key, app_status);
    }

    Ok(())
}

pub async fn refresh_logs_from_state_files() -> Result<(), ErrorArrayItem> {
    let keys: Vec<Stringy> = {
        let status_array = APP_STATUS_ARRAY
            .try_read()
            .await
            .map_err(|err| ErrorArrayItem::new(Errors::TimedOut, err.to_string()))?;
        status_array.keys().cloned().collect()
    };

    let mut updates: Vec<(Stringy, Vec<(u64, String)>, Vec<(u64, String)>)> = Vec::new();
    for key in keys {
        let app_name = key.to_string();
        let Some(state) = load_app_state(&app_name).await else {
            continue;
        };
        updates.push((key, state.stdout, state.stderr));
    }

    if updates.is_empty() {
        return Ok(());
    }

    let mut status_array = APP_STATUS_ARRAY
        .try_write()
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::TimedOut, err.to_string()))?;

    for (key, stdout, stderr) in updates {
        let Some(app_status) = status_array.get_mut(&key) else {
            continue;
        };
        app_status.app_data.state.stdout = stdout;
        app_status.app_data.state.stderr = stderr;
    }

    Ok(())
}
