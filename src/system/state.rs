use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::core::{
    errors::{ErrorArrayItem, Errors},
    functions::current_timestamp,
    logger::LogLevel,
};
use artisan_middleware::{
    config::AppConfig,
    dusa_collection_utils::core::types::pathtype::PathType,
    state_persistence::{self, AppState, StatePersistence},
};

const WATCHDOG_CONNECTED_PREFIX: &str = "watchdog_connected";

pub fn get_state_path(config: &AppConfig) -> PathType {
    state_persistence::StatePersistence::get_state_path(&config)
}

pub fn upsert_watchdog_connection_marker(data: &str, socket_path: Option<&str>) -> String {
    let mut retained: Vec<String> = Vec::new();

    for raw_line in data.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some((key, _)) = line.split_once(':').or_else(|| line.split_once('=')) {
            if key.trim() == WATCHDOG_CONNECTED_PREFIX {
                continue;
            }
        }

        retained.push(raw_line.to_string());
    }

    if let Some(socket_path) = socket_path {
        let socket = socket_path.trim();
        if !socket.is_empty() {
            retained.push(format!("{WATCHDOG_CONNECTED_PREFIX}:{socket}"));
        }
    }

    retained.join("\n")
}

pub async fn save_state(state: &mut AppState, path: &PathType) {
    state.last_updated = current_timestamp();
    state.event_counter += 1;
    if let Err(err) = StatePersistence::save_state(state, path).await {
        log!(LogLevel::Error, "Failed to save state: {}", err);
        state.error_log.push(ErrorArrayItem::new(
            Errors::GeneralError,
            format!("{}", err),
        ));
    }
}

// Update the state file in the case of a un handled error
pub async fn _wind_down_state(state: &mut AppState, state_path: &PathType) {
    state.data = String::from("Terminated");
    state.last_updated = current_timestamp();
    state.error_log.push(ErrorArrayItem::new(
        Errors::GeneralError,
        "Wind down requested check logs".to_owned(),
    ));
    save_state(state, &state_path).await;
}
