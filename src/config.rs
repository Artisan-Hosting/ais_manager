use artisan_middleware::{
    config::{Aggregator, AppConfig}, state_persistence::{self, AppState, StatePersistence}, timestamp::current_timestamp
};
use dusa_collection_utils::{
    log::{set_log_level, LogLevel},
    log,
    stringy::Stringy,
    types::PathType,
    version::SoftwareVersion,
};

use crate::update_state;

pub fn get_config() -> AppConfig {
    match artisan_middleware::config::AppConfig::new() {
        Ok(mut data_loaded) => {
            data_loaded.git = None;
            data_loaded.database = None;
            data_loaded.app_name = Stringy::from_string(env!("CARGO_PKG_NAME").to_string());
            data_loaded.version = SoftwareVersion::dummy().to_string();
            data_loaded.aggregator = Some(Aggregator{ socket_path: "/tmp/test.sock".into(), socket_permission: Some(755) });
            data_loaded
        }
        Err(e) => {
            log!(LogLevel::Error, "Error loading config: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn get_state_path(config: &AppConfig) -> PathType {
    state_persistence::StatePersistence::get_state_path(&config)
}

pub async fn generate_initial_state(config: &AppConfig) -> AppState {
    let state_path: PathType = get_state_path(&config);
    
    match StatePersistence::load_state(&state_path).await {
        Ok(mut loaded_data) => {
            log!(LogLevel::Info, "Loaded previous state data");
            log!(LogLevel::Trace, "Previous state data: {:#?}", loaded_data);
            loaded_data.is_active = false;
            loaded_data.data = String::from("Initializing");
            loaded_data.config.debug_mode = config.debug_mode;
            loaded_data.last_updated = current_timestamp();
            loaded_data.config.log_level = config.log_level;
            set_log_level(loaded_data.config.log_level);
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            loaded_data.error_log.clear();
            update_state(&mut loaded_data, &state_path).await;
            loaded_data
        }
        Err(e) => {
            log!(LogLevel::Warn, "No previous state loaded, creating new one");
            log!(LogLevel::Debug, "Error loading previous state: {}", e);
            let mut state = AppState {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: SoftwareVersion::dummy(),
                data: String::new(),
                last_updated: current_timestamp(),
                event_counter: 0,
                is_active: false,
                error_log: vec![],
                config: config.clone(),
                system_application: true,
            };
            state.is_active = false;
            state.data = String::from("Initializing");
            state.config.debug_mode = true;
            state.last_updated = current_timestamp();
            state.config.log_level = config.log_level;
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            state.error_log.clear();
            update_state(&mut state, &state_path).await;
            state
        }
    }
}
