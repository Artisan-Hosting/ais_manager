use artisan_middleware::dusa_collection_utils::{
    log,
    logger::{set_log_level, LogLevel},
    version::{SoftwareVersion, Version, VersionCode},
};
use artisan_middleware::{
    aggregator::Status,
    config::AppConfig,
    dusa_collection_utils::types::{pathtype::PathType, stringy::Stringy},
    state_persistence::{AppState, StatePersistence},
    timestamp::current_timestamp,
    version::{aml_version, str_to_version},
};

use crate::system::state::save_state;

use super::state::get_state_path;

pub fn get_config() -> AppConfig {
    match artisan_middleware::config::AppConfig::new() {
        Ok(mut data_loaded) => {
            // data_loaded.git = None;
            data_loaded.database = None;
            data_loaded.app_name = Stringy::from(env!("CARGO_PKG_NAME").to_string());
            // data_loaded.aggregator = Some(Aggregator{ socket_path: "/tmp/test.sock".into(), socket_permission: Some(755) });
            data_loaded
        }
        Err(e) => {
            log!(LogLevel::Error, "Error loading config: {}", e);
            std::process::exit(1);
        }
    }
}

pub async fn generate_state(config: &AppConfig) -> AppState {
    let state_path: PathType = get_state_path(&config);

    match StatePersistence::load_state(&state_path).await {
        Ok(mut loaded_data) => {
            log!(LogLevel::Info, "Loaded previous state data");
            // log!(LogLevel::Trace, "Previous state data: {:#?}", loaded_data);
            loaded_data.data = String::from("Initializing");
            loaded_data.config.debug_mode = config.debug_mode;
            loaded_data.last_updated = current_timestamp();
            loaded_data.config.log_level = config.log_level;
            loaded_data.config.aggregator = config.aggregator.clone();
            loaded_data.config.environment = config.environment.clone();
            loaded_data.stared_at = current_timestamp();
            loaded_data.version = {
                let library_version: Version = aml_version();
                let software_version: Version =
                    str_to_version(env!("CARGO_PKG_VERSION"), Some(VersionCode::Production));

                SoftwareVersion {
                    application: software_version,
                    library: library_version,
                }
            };
            loaded_data.pid = std::process::id();
            set_log_level(loaded_data.config.log_level);
            loaded_data.event_counter = 0;
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            loaded_data.error_log.clear();
            save_state(&mut loaded_data, &state_path).await;
            loaded_data
        }
        Err(e) => {
            log!(LogLevel::Warn, "No previous state loaded, creating new one");
            log!(LogLevel::Debug, "Error loading previous state: {}", e);
            let mut state = AppState {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: {
                    let library_version: Version = aml_version();
                    let software_version: Version =
                        str_to_version(env!("CARGO_PKG_VERSION"), Some(VersionCode::Production));

                    SoftwareVersion {
                        application: software_version,
                        library: library_version,
                    }
                },
                data: String::new(),
                last_updated: current_timestamp(),
                event_counter: 0,
                pid: std::process::id(),
                error_log: vec![],
                config: config.clone(),
                system_application: true,
                stared_at: current_timestamp(),
                status: Status::Running,
            };
            state.data = String::from("Initializing");
            state.config.debug_mode = true;
            state.last_updated = current_timestamp();
            state.config.log_level = config.log_level;
            state.config.environment = config.environment.clone();
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            state.error_log.clear();
            save_state(&mut state, &state_path).await;
            state
        }
    }
}
