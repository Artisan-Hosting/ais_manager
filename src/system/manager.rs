use artisan_middleware::{
    dusa_collection_utils::{
        core::errors::{ErrorArrayItem, Errors},
        core::functions::current_timestamp,
        core::types::pathtype::PathType,
    },
    git_actions::{GitAuth, GitCredentials},
    portal::ManagerData,
    state_persistence::AppState,
};
use simple_comms::network::utils::get_local_ip;

use crate::applications::child::{
    APP_STATUS_ARRAY, CLIENT_APPLICATION_ARRAY, SYSTEM_APPLICATION_ARRAY,
};

use gethostname::gethostname;

use super::portal::load_identifier;

pub async fn get_manager_data(state: &mut AppState) -> Result<ManagerData, ErrorArrayItem> {
    let manager_version = state.version.clone();

    let git_credentials: GitCredentials = if let Some(config) = &state.config.git {
        let cred_array: Vec<GitAuth> =
            GitCredentials::new_vec(Some(&PathType::Str(config.credentials_file.clone().into())))
                .await?;
        let credentials: GitCredentials = GitCredentials {
            auth_items: cred_array,
        };
        credentials
    } else {
        return Err(ErrorArrayItem::new(
            Errors::ConfigParsing,
            "Failed to parse the git repos file on the manager",
        ));
    };

    let system_array = SYSTEM_APPLICATION_ARRAY.try_read().await?;
    let client_array = CLIENT_APPLICATION_ARRAY.try_read().await?;
    let status_array = APP_STATUS_ARRAY.try_read().await?;
    let mut uptime = None;

    status_array.clone().into_iter().for_each(|status| {
        if status.1.app_id == "ais_manager".into() {
            uptime = status.1.uptime
        }
    });

    let system_warning_count = {
        let mut num = 0;
        for system in system_array.clone() {
            let state = system.1.config.get_state();
            let count = state.error_log.len();
            num += count
        }
        num
    };

    let client_warning_count = {
        let mut num = 0;
        for client in client_array.clone() {
            let state = client.1.config.get_state();
            let count = state.error_log.len();
            num += count
        }
        num
    };

    let identity = if let Some(id) = load_identifier().await {
        id
    } else {
        return Err(ErrorArrayItem::new(
            Errors::AuthenticationError,
            "No identity data found on system",
        ));
    };

    let manager_data = ManagerData {
        version: manager_version,
        git_config: git_credentials,
        system_apps: system_array.len() as u32,
        client_apps: client_array.len() as u32,
        warning: (client_warning_count + system_warning_count) as u32,
        hostname: match gethostname().into_string() {
            Ok(data) => data.into(),
            Err(_) => "Failed to resolve hostname".into(),
        },
        identity,
        address: std::net::IpAddr::V4(get_local_ip()),
        uptime: { current_timestamp() - state.stared_at },
    };

    Ok(manager_data)
}
