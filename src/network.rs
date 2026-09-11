//! The manager's inbound command surface, and the one place a command is
//! actually *executed*.
//!
//! The manager answers commands on two channels that look nothing alike:
//!
//! - The **fleet tunnel** (`system::portal`): one persistent, full-duplex
//!   connection the manager dials out to the portal on `:9800`, carrying
//!   correlated requests indefinitely. This is how the portal reaches this
//!   node, and in production it is the only one that matters.
//! - The **local debug listener** ([`process_tcp`]): plain request/response on
//!   `127.0.0.1:9825`, one command per connection, for an operator on the box.
//!
//! They share [`command_processor`] verbatim -- neither channel interprets a
//! command itself. That is the point: what `ais_manager_debug` sees is what
//! the portal would have seen, so a behavioral question can be answered
//! locally without involving the fleet at all.
//!
//! Note the asymmetry in who dials whom. The tunnel is outbound, so nothing in
//! this file participates in it; the listener below is inbound but bound to
//! loopback, so nothing off-host can reach it either. A node has no
//! externally-reachable port. See `system::ports`.

use artisan_middleware::dusa_collection_utils::{core::errors::ErrorArrayItem, log};
use artisan_middleware::{
    aggregator::{AppMessage, Command, CommandResponse, CommandType},
    config::AppConfig,
    dusa_collection_utils::core::{
        logger::LogLevel,
        types::{pathtype::PathType, stringy::Stringy},
    },
    portal::ManagerData,
    state_persistence::AppState,
};
use simple_comms::network::send_receive::{establish_connection_responder, receive_message};
use simple_comms::protocol::flags::MsgType;
use simple_comms::protocol::handshake::NoiseIdentity;
use simple_comms::protocol::message::ConnectionCtx;
use simple_comms::{
    network::send_receive::send_empty_err,
    protocol::{message::ProtocolMessage, proto::Proto},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpStream;

use crate::{
    applications::child::APP_STATUS_ARRAY,
    system::{control::Controls, manager::get_manager_data},
    watchdog,
};

/// Handles one connection on the manager's **local, loopback-only** debug
/// listener ([`crate::system::ports::DEBUG_LISTENER_ADDR`],
/// `127.0.0.1:9825`), used by `ais_manager_debug` for manual
/// diagnostics/overrides on the same host. This is no longer part of the
/// fleet protocol -- the portal reaches this manager over the persistent
/// tunnel it dials out on (`system::portal::maintain_tunnel`), which shares
/// the same command execution logic via [`command_processor`] but never
/// touches this listener.
///
/// One connection here carries exactly one command: this is plain
/// request/response (`send_receive::send_message`), not the tunnel's
/// full-duplex `ConnectionDriver`, so there is no correlation id and no
/// long-lived state -- handshake, one request, one reply, close.
pub async fn process_tcp(
    mut connection: (TcpStream, SocketAddr),
    application_controls: Arc<Controls>,
    state: &mut AppState,
    _state_path: &PathType,
    _config: &AppConfig,
    identity: &NoiseIdentity,
) -> Result<(), ErrorArrayItem> {
    let proto: Proto = Proto::TCP;

    // The manager is the *responder* on this listener: `connection` came off
    // our own loopback listener and the debug CLI dialled in. A Noise_NK
    // responder authenticates itself with its long-term identity and adopts
    // whatever ConnectionParams baseline the initiator declares on `Hello`,
    // so there is nothing to negotiate here.
    let mut connctx: ConnectionCtx =
        establish_connection_responder(&mut connection.0, identity).await?;

    // `auto_reply` is false because every branch below sends its own reply.
    //
    // The hand-rolled "did the peer use weak flags?" SIDEGRADE check that used
    // to live here is now the library's job: the baseline is fixed at handshake
    // time and `receive_message` compares each message against `conn.params`
    // itself. See simple_comms' docs/HANDSHAKE.md.
    let request: AppMessage =
        receive_message::<_, AppMessage>(&mut connection.0, false, proto, Some(&mut connctx))
            .await?
            .payload;

    match request {
        AppMessage::Command(command) => {
            let app_id = command.app_id.clone();
            let command_type = command.command_type.clone();

            let payload = match command_processor(command, application_controls, state).await {
                Ok(data) => data,
                Err(err) => AppMessage::Response(CommandResponse {
                    app_id,
                    command_type,
                    success: false,
                    message: Some(err.to_string()),
                }),
            };

            let message: ProtocolMessage<AppMessage> =
                ProtocolMessage::new(connctx.params, MsgType::Data, payload)?;

            message.write_to(&mut connection.0, proto, Some(&mut connctx)).await?;
        }

        _ => {
            // * illegal in this context
            send_empty_err(&mut connection.0, proto).await?;
            return Ok(());
        }
    }

    Ok(())
}

/// Executes one [`Command`] and produces the [`AppMessage`] to send back.
///
/// **This is the single implementation of what a command *means* to the
/// manager**, shared verbatim by both channels: the fleet tunnel
/// (`system::portal::run_dispatch_loop`) and the local debug listener
/// ([`process_tcp`]). Neither channel interprets commands itself, which is
/// what makes `ais_manager_debug` a faithful stand-in for the portal when
/// diagnosing behavior -- the only difference between them is transport.
///
/// Lifecycle commands (`Start`/`Stop`/`Restart`) are proxied to watchdog,
/// which owns process lifecycle; the manager only relays the verdict.
/// `Status`/`AllStatus` are served from the in-memory cache that
/// `applications::watchdog_sync` refreshes, so they never block on watchdog.
/// `ais_manager` naming itself as the target of a stop/restart is special
/// cased into an internal reload, since a manager cannot usefully be asked to
/// stop the process answering the request.
///
/// Errors are reported *in band* wherever possible -- a `CommandResponse`
/// with `success: false` -- rather than as an `Err`, so a failed command
/// never looks like a failed connection to the caller.
pub(crate) async fn command_processor(
    command: Command,
    application_controls: Arc<Controls>,
    state: &mut AppState,
) -> Result<AppMessage, ErrorArrayItem> {
    if let Err(err) = application_controls
        .wait_for_network_control_with_timeout(Duration::from_secs(1))
        .await
    {
        log!(LogLevel::Error, "{}", err);
        return Ok(AppMessage::Response(CommandResponse {
            app_id: "".into(),
            command_type: CommandType::Custom("Unknown".into()),
            success: false,
            message: Some("Server not accepting requests".to_owned()),
        }));
    }

    let app_id: Stringy = command.app_id;
    match command.command_type {
        artisan_middleware::aggregator::CommandType::Start => {
            match watchdog::execute_start(&app_id.to_string()).await {
                Ok(response) => Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Start,
                    success: response.accepted,
                    message: match response.message.trim() {
                        "" => None,
                        msg => Some(msg.to_owned()),
                    },
                })),
                Err(err) => Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Start,
                    success: false,
                    message: Some(format!("Watchdog unavailable: {}", err)),
                })),
            }
        }
        artisan_middleware::aggregator::CommandType::Stop => {
            if app_id == "ais_manager".into() {
                application_controls.signal_reload();
                return Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: true,
                    message: Some("triggered reload !".to_owned()),
                }));
            }

            match watchdog::execute_stop(&app_id.to_string()).await {
                Ok(response) => Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Stop,
                    success: response.accepted,
                    message: match response.message.trim() {
                        "" => None,
                        msg => Some(msg.to_owned()),
                    },
                })),
                Err(err) => Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Stop,
                    success: false,
                    message: Some(format!("Watchdog unavailable: {}", err)),
                })),
            }
        }
        artisan_middleware::aggregator::CommandType::Restart => {
            // Check if the request is a self restart first
            if app_id == "ais_manager".into() {
                application_controls.signal_reload();
                return Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: true,
                    message: None,
                }));
            }

            match watchdog::execute_reload(&app_id.to_string()).await {
                Ok(response) => Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: response.accepted,
                    message: match response.message.trim() {
                        "" => None,
                        msg => Some(msg.to_owned()),
                    },
                })),
                Err(err) => Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: false,
                    message: Some(format!("Watchdog unavailable: {}", err)),
                })),
            }
        }
        artisan_middleware::aggregator::CommandType::Status => {
            let store_lock = APP_STATUS_ARRAY
                .try_read_with_timeout(Some(Duration::from_secs(5)))
                .await?;

            if store_lock.contains_key(&app_id) {
                match store_lock.get(&app_id) {
                    Some(app) => {
                        let mut app = app.clone();
                        app.timestamp = 0;

                        // Truncate stdout and stderr to the latest 20 lines
                        let stdout_len = app.app_data.state.stdout.len();
                        if stdout_len > 20 {
                            app.app_data.state.stdout = app.app_data.state.stdout[stdout_len - 20..].to_vec();
                        }
                        let stderr_len = app.app_data.state.stderr.len();
                        if stderr_len > 20 {
                            app.app_data.state.stderr = app.app_data.state.stderr[stderr_len - 20..].to_vec();
                        }

                        let response_data = AppMessage::Response(CommandResponse {
                            app_id,
                            command_type: CommandType::Status,
                            success: true,
                            message: app.to_json(),
                        });
                        return Ok(response_data);
                    }
                    None => {
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id: app_id.clone(),
                            command_type: CommandType::Status,
                            success: false,
                            message: Some(format!("The app: {}, wasn't in our store", app_id)),
                        }));
                    }
                }
            }

            drop(store_lock);

            return Ok(AppMessage::Response(CommandResponse {
                app_id: app_id.clone(),
                command_type: CommandType::Status,
                success: false,
                message: Some(format!("The app: {}, wasn't in our store", app_id)),
            }));
        }
        artisan_middleware::aggregator::CommandType::AllStatus => {
            let store_lock = APP_STATUS_ARRAY
                .try_read_with_timeout(Some(Duration::from_secs(2)))
                .await?;

            let mut status_vec = Vec::new();

            for (id, status) in store_lock.iter() {
                log!(LogLevel::Debug, "Sending status of: {}", id);
                let mut status_clone = status.clone();

                // Truncate stdout and stderr to the latest 20 lines
                let stdout_len = status_clone.app_data.state.stdout.len();
                if stdout_len > 20 {
                    status_clone.app_data.state.stdout = status_clone.app_data.state.stdout[stdout_len - 20..].to_vec();
                }
                let stderr_len = status_clone.app_data.state.stderr.len();
                if stderr_len > 20 {
                    status_clone.app_data.state.stderr = status_clone.app_data.state.stderr[stderr_len - 20..].to_vec();
                }

                status_vec.push(status_clone.to_json().unwrap());
            }

            status_vec.shrink_to_fit();
            let mut data = String::new();

            for status in status_vec {
                data.push_str(&format!("{},", status));
            }

            let response_data = AppMessage::Response(CommandResponse {
                app_id,
                command_type: CommandType::AllStatus,
                success: true,
                message: Some(format!("[{}]", data).replace(",]", "]")),
            });

            drop(store_lock);

            return Ok(response_data);
        }

        artisan_middleware::aggregator::CommandType::Info => {
            let manager_data: ManagerData = get_manager_data(state).await?;
            return Ok(AppMessage::ManagerInfo(manager_data));
        }

        // `Custom` is the extension point for verbs the shared crate does not
        // model. The payload rides after the first space because
        // `Command` has nowhere else to put one -- it carries only `app_id`,
        // `command_type` and `timestamp`, and the wire is bincode of those, so
        // adding a real body field would break every peer at once.
        artisan_middleware::aggregator::CommandType::Custom(ref raw) => {
            let (verb, body) = match raw.split_once(' ') {
                Some((verb, body)) => (verb, body.trim()),
                None => (raw.as_str(), ""),
            };

            if crate::system::git_repos::handles(verb) {
                return Ok(crate::system::git_repos::handle(verb, body, &state.config).await);
            }

            return Ok(AppMessage::Response(CommandResponse {
                app_id,
                command_type: CommandType::Custom("command not found".to_string()),
                success: false,
                message: Some("Request not implemented".into()),
            }));
        }
    }
}
