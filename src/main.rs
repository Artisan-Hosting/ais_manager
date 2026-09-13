use applications::{
    child::upsert_local_manager_state,
    watchdog_sync::{refresh_logs_from_state_files, refresh_status_from_watchdog},
};
use artisan_middleware::dusa_collection_utils::core::{
    errors::ErrorArrayItem,
    logger::LogLevel,
    types::{pathtype::PathType, rwarc::LockWithTimeout, stringy::Stringy},
};
use artisan_middleware::{
    aggregator::AppStatus,
    config::AppConfig,
    state_persistence::{AppState, StatePersistence},
};
use artisan_middleware::{dusa_collection_utils::log, identity::Identifier};
use network::process_tcp;
use simple_comms::protocol::handshake::NoiseIdentity;
use std::{collections::HashMap, sync::Arc, time::Duration};
use system::{
    config::{generate_state, get_config},
    control::Controls,
    noise::{load_or_create_identity, load_portal_pubkey},
    portal::maintain_tunnel,
    ports::DEBUG_LISTENER_ADDR,
    state::{get_state_path, save_state},
};
use tokio::{net::TcpListener, time::sleep};

mod applications;
mod network;
mod secrets;
mod system;
mod watchdog;

pub type AppStatusArray = LockWithTimeout<HashMap<Stringy, AppStatus>>;

#[tokio::main]
async fn main() -> Result<(), ErrorArrayItem> {
    // loading configuration and state persistence
    let config: AppConfig = get_config();
    let mut state: AppState = generate_state(&config).await;
    let state_path: PathType = get_state_path(&config);
    if config.debug_mode {
        log!(LogLevel::Debug, "\n{}", state);
    }

    // Noise_NK key material. Loaded once here and shared, so neither the
    // tunnel supervisor nor an inbound debug connection re-reads it per
    // connection.
    //
    // `identity` authenticates the *local, loopback-only* debug listener
    // (`network::process_tcp`) -- we are the responder there. `portal_key` is
    // the portal's, pinned so we can authenticate it as the responder when we
    // dial the fleet tunnel. See src/system/noise.rs.
    let identity: Arc<NoiseIdentity> = Arc::new(load_or_create_identity()?);
    let portal_key: Arc<[u8; 32]> = Arc::new(load_portal_pubkey()?);
    log!(
        LogLevel::Info,
        "Manager debug-listener Noise identity loaded; its public key is {}",
        hex::encode(identity.public_key())
    );

    {
        if let Err(_) = Identifier::load_from_file() {
            log!(LogLevel::Warn, "Creating new machine id");
            let id = Identifier::new().await.unwrap();
            id.save_to_file().unwrap();
        }
    }
    {
        upsert_local_manager_state(&state).await?;
        if let Err(err) = refresh_status_from_watchdog().await {
            log!(LogLevel::Warn, "Initial watchdog inventory sync failed: {}", err);
        }
    }

    // seting up trackers
    match Controls::initialize_controls().await {
        Ok(controls) => Arc::new(controls),
        Err(err) => return Err(err),
    };

    let application_controls: Arc<Controls> = Controls::get_controls().await?;
    

    // setting up controls and signal monitoring
    application_controls.start_signal_monitors();
    application_controls
        .clone()
        .start_contol_monitor();

    // Sync inventory/status/metrics from watchdog.
    tokio::spawn(async move {
        loop {
            if let Err(err) = refresh_status_from_watchdog().await {
                log!(LogLevel::Warn, "Watchdog status sync failed: {}", err);
            }
            sleep(Duration::from_secs(2)).await;
        }
    });

    // Refresh logs from per-app state files (client-app direct stdout/stderr).
    tokio::spawn(async move {
        loop {
            if let Err(err) = refresh_logs_from_state_files().await {
                log!(LogLevel::Warn, "State-file log refresh failed: {}", err);
            }
            sleep(Duration::from_secs(5)).await;
        }
    });

    // Periodically re-run the same force-resync/force-clean/state-file-purge
    // logic `GitReposAudit` exposes on demand (see system::git_repos::run_audit),
    // so a node's checkouts and /opt/artisan/tmp state files self-heal even if
    // nobody ever clicks the portal's "Recent Repositories" button. An hour is
    // deliberately not aggressive: this does real git network traffic against
    // every configured repo, and a write already triggers the same sync/clean
    // as a side effect, so this periodic pass is a safety net for drift
    // between edits, not the primary mechanism.
    let audit_config = config.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(3600)).await;
            match crate::system::git_repos::run_audit(&audit_config).await {
                Ok(outcome) if !outcome.errors.is_empty() => {
                    log!(
                        LogLevel::Warn,
                        "Periodic git repo audit finished with errors: {:?}",
                        outcome.errors
                    );
                }
                Ok(outcome) => {
                    log!(
                        LogLevel::Debug,
                        "Periodic git repo audit: {} repo(s) considered, {} stale checkout(s) removed, {} stale state file(s) removed",
                        outcome.repos_considered,
                        outcome.stale_checkouts_removed,
                        outcome.stale_state_files_removed
                    );
                }
                Err(err) => {
                    log!(LogLevel::Warn, "Periodic git repo audit failed: {}", err);
                }
            }
        }
    });

    // Periodically pull each locally-hosted git-backed app's full secret-server
    // KV set and relay it to watchdog to fold into that app's runtime bundle
    // (Phase E, E10). Secret-server, not this bundle, stays the authoritative
    // read/write boundary for the dashboard's Secrets page -- this loop only
    // ever reads from secret-server and pushes down, never the other
    // direction. Scoped to git-backed client apps for now, matching E9's
    // generic_runner-only bundle-consumption scope; the fixed system apps
    // (ais_manager, ais_gitmon, ais_mailler) keep their legacy config path.
    let secrets_sync_config = config.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(300)).await;

            let path = crate::system::git_repos::resolve_path(&secrets_sync_config);
            let credentials = crate::system::git_repos::load(&path).await;

            for auth in &credentials.auth_items {
                let bare_id = auth.generate_id().to_string();
                let application = format!("ais_{}", bare_id);

                let environment = match crate::watchdog::get_app_environment(&application).await {
                    Ok(Some(env)) => env,
                    Ok(None) => continue, // no runtime bundle yet; nothing to sync
                    Err(err) => {
                        log!(
                            LogLevel::Warn,
                            "Secrets sync: reading environment for '{}' failed: {}",
                            application,
                            err
                        );
                        continue;
                    }
                };

                let mut client = match crate::secrets::SecretClient::connect().await {
                    Ok(client) => client,
                    Err(err) => {
                        log!(LogLevel::Warn, "Secrets sync: connecting to secret-server failed: {}", err);
                        break; // secret-server is unreachable; retry next interval
                    }
                };

                let content = match client.get_all_as_env_lines(&bare_id, &environment).await {
                    Ok(content) => content,
                    Err(err) => {
                        log!(
                            LogLevel::Warn,
                            "Secrets sync: fetching secrets for '{}' failed: {}",
                            application,
                            err
                        );
                        continue;
                    }
                };

                if let Err(err) = crate::watchdog::set_bundle_env(&application, &content).await {
                    log!(
                        LogLevel::Warn,
                        "Secrets sync: pushing bundle env for '{}' failed: {}",
                        application,
                        err
                    );
                }
            }
        }
    });

    // Keep the watchdog connection marker fresh for watchdog `manager_linked` detection.
    let state_path_for_watchdog = state_path.clone();
    tokio::spawn(async move {
        let mut was_connected = false;
        loop {
            let connected = crate::watchdog::is_reachable(Duration::from_millis(800)).await;
            let mut sleep_for = Duration::from_secs(if connected { 10 } else { 30 });

            let should_persist = connected || connected != was_connected;
            if should_persist {
                let manager_state = match StatePersistence::load_state(&state_path_for_watchdog).await
                {
                    Ok(state) => Some(state),
                    Err(_) => None,
                };

                if let Some(mut manager_state) = manager_state {
                    manager_state.data = crate::system::state::upsert_watchdog_connection_marker(
                        &manager_state.data,
                        connected.then_some(crate::watchdog::socket_path()),
                    );
                    crate::system::state::save_state(&mut manager_state, &state_path_for_watchdog)
                        .await;
                } else {
                    sleep_for = Duration::from_secs(1);
                }
            }

            was_connected = connected;
            sleep(sleep_for).await;
        }
    });

    // The fleet tunnel: one persistent, full-duplex connection to the portal,
    // carrying registration once and then command traffic indefinitely. See
    // system::portal for the supervisor/redial logic.
    let state_clone = state.clone();
    let application_controls_clone = application_controls.clone();
    let portal_key_clone = portal_key.clone();
    tokio::spawn(async move {
        maintain_tunnel(state_clone, application_controls_clone, portal_key_clone).await;
    });

    // Local, loopback-only debug listener for `ais_manager_debug` (:9825) --
    // no longer part of the fleet protocol (see src/system/ports.rs and
    // src/system/noise.rs). Everything the portal does reaches us over the
    // tunnel above; this exists purely so an operator on the box can drive the
    // same `command_processor` by hand.
    let tcp_listener: TcpListener = TcpListener::bind(DEBUG_LISTENER_ADDR)
        .await
        .map_err(|err| ErrorArrayItem::from(err))?;
    log!(
        LogLevel::Info,
        "Local debug listener bound to {} (loopback only)",
        DEBUG_LISTENER_ADDR
    );

    let state_path_clone = state_path.clone();
    let config_clone = config.clone();
    loop {
        tokio::select! {
            Ok(conn) = tcp_listener.accept() => {
                let app_controls = application_controls.clone();
                let mut state_clone = state.clone();
                let state_path_clone = state_path_clone.clone();
                let config_clone = config_clone.clone();
                let identity_clone = identity.clone();

                state.event_counter += 1;
                save_state(&mut state, &state_path_clone).await;
                tokio::spawn(async move {
                    if let Err(err) = process_tcp(conn, app_controls, &mut state_clone, &state_path_clone, &config_clone, &identity_clone).await {
                        log!(LogLevel::Error, "TCP connection handling panicked: {:?}", err);
                    }
                });
            }
        }
    }
}
