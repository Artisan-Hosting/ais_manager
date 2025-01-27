use artisan_middleware::{config::AppConfig, state_persistence::{self, AppState, StatePersistence}};
use artisan_middleware::dusa_collection_utils::{errors::{ErrorArrayItem, Errors}, functions::current_timestamp, types::PathType};
use artisan_middleware::dusa_collection_utils::log::LogLevel;
use artisan_middleware::dusa_collection_utils::log;

pub fn get_state_path(config: &AppConfig) -> PathType {
    state_persistence::StatePersistence::get_state_path(&config)
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
