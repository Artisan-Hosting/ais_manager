use artisan_middleware::{
    dusa_collection_utils::{
        core::{
            errors::{ErrorArrayItem, Errors},
            functions::current_timestamp,
            types::pathtype::PathType,
        },
    },
    git_actions::{GitAuth, GitCredentials},
    portal::ManagerData,
    state_persistence::AppState,
};
use simple_comms::network::utils::get_local_ip;

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::applications::child::APP_STATUS_ARRAY;

use gethostname::gethostname;

use super::portal::load_identifier;

static WATCHDOG_SECURITY_TRIPPED_EVER: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

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

    let status_array = APP_STATUS_ARRAY.try_read().await?;
    let mut system_apps: u32 = 0;
    let mut client_apps: u32 = 0;
    for (_, status) in status_array.iter() {
        if status.app_data.is_system_application() {
            system_apps += 1;
        } else {
            client_apps += 1;
        }
    }

    if let Ok(status) = crate::watchdog::get_security_trip_status().await {
        if status.tripped {
            WATCHDOG_SECURITY_TRIPPED_EVER.store(true, Ordering::Relaxed);
        }
    }
    let watchdog_security_warning = if WATCHDOG_SECURITY_TRIPPED_EVER.load(Ordering::Relaxed) {
        1
    } else {
        0
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
        system_apps,
        client_apps,
        // NOTE: The portal warning count is now reserved for watchdog security/tamper trips only.
        // Watchdog derives `manager_linked` separately via the manager state-file marker.
        warning: watchdog_security_warning,
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
