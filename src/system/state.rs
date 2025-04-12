use std::sync::Arc;

use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::{
    errors::{ErrorArrayItem, Errors},
    functions::current_timestamp,
};
use artisan_middleware::{
    config::AppConfig,
    dusa_collection_utils::types::pathtype::PathType,
    state_persistence::{self, AppState, StatePersistence},
};

use super::control::{GlobalState, GLOBAL_STATE};

pub fn get_state_path(config: &AppConfig) -> PathType {
    state_persistence::StatePersistence::get_state_path(&config)
}

pub async fn save_state(state: &mut AppState, path: &PathType) -> Result<(), ErrorArrayItem> {
    let global_state: Option<&Arc<GlobalState>> = GLOBAL_STATE.get();

    state.last_updated = current_timestamp();
    state.event_counter += 1;

    // Update global state
    if let Some(gs) = global_state {
        let mut app_state = gs
            .app_state
            .try_write()
            .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        *app_state = state.clone();
    }

    // Writing out to file
    if let Err(err) = StatePersistence::save_state(state, path).await {
        log!(LogLevel::Error, "Failed to save state: {}", err);
        state.error_log.push(ErrorArrayItem::new(
            Errors::GeneralError,
            format!("{}", err),
        ));
    }

    Ok(())
}

// Update the state file in the case of a un handled error
pub async fn wind_down_state(state: &mut AppState, state_path: &PathType) -> Result<(), ErrorArrayItem> {
    state.data = String::from("Terminated");
    state.last_updated = current_timestamp();
    state.error_log.push(ErrorArrayItem::new(
        Errors::GeneralError,
        "Wind down requested check logs".to_owned(),
    ));
    save_state(state, &state_path).await
}
