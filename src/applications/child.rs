use artisan_middleware::dusa_collection_utils::core::{
    errors::ErrorArrayItem,
    types::{rwarc::LockWithTimeout, stringy::Stringy},
};
use artisan_middleware::dusa_collection_utils::platform::functions::{create_hash, truncate};
use artisan_middleware::identity::Identifier;
use artisan_middleware::{
    aggregator::{AppStatus, Status},
    config_bundle::ApplicationConfig,
    state_persistence::AppState,
};
use once_cell::sync::Lazy;
use std::{collections::HashMap, time::Duration};

pub static APP_STATUS_ARRAY: Lazy<LockWithTimeout<HashMap<Stringy, AppStatus>>> =
    Lazy::new(|| LockWithTimeout::new(HashMap::new()));

pub fn make_app_status(
    app_name: &str,
    app_config: ApplicationConfig,
) -> Result<(Stringy, AppStatus), ErrorArrayItem> {
    let identity: Identifier = Identifier::load_from_file()?;
    let key: Stringy = app_name.into();

    let app_id: Stringy = {
        let data = format!("{}-{}", identity.id, app_name);
        let hash = create_hash(data);
        truncate(&*hash, 20).to_owned()
    };

    let git_id: Stringy = if app_config.is_system_application() {
        "".into()
    } else {
        app_name.replace("ais_", "").into()
    };

    let expected_status = if app_config.is_system_application() {
        Status::Running
    } else {
        Status::Idle
    };

    let app_status: AppStatus = AppStatus {
        app_id,
        git_id,
        app_data: app_config,
        uptime: None,
        metrics: None,
        timestamp: 0,
        expected_status,
    };

    Ok((key, app_status))
}

pub async fn upsert_local_manager_state(state: &AppState) -> Result<(), ErrorArrayItem> {
    let app_name = "ais_manager";
    let config = ApplicationConfig::new(state.clone(), None, None);
    let (key, mut app_status) = make_app_status(app_name, config)?;
    app_status.timestamp = state.last_updated;

    let mut status_array = APP_STATUS_ARRAY
        .try_write_with_timeout(Some(Duration::from_secs(2)))
        .await?;
    status_array.insert(key, app_status);
    Ok(())
}
