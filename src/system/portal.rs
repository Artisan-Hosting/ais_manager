//! The manager's single full-duplex tunnel to the portal: one persistent,
//! `Noise_NK`-secured, manager-initiated connection to the portal's fleet
//! port ([`crate::system::ports::PORTAL_TUNNEL_PORT`], `:9800`) that carries a
//! one-time registration bootstrap followed by indefinite command traffic in
//! both directions. See `portal_upstream_api.md` §2/§2.1 and §7,
//! `simple_comms`' `docs/HANDSHAKE.md` (`ConnectionDriver` section), and
//! `portal/docs/fleet_tunnel.md` for the portal's side of the same connection.
//!
//! The manager is a Noise *initiator only* here -- it dials out, so it needs
//! only the portal's pinned public key, never a static identity of its own
//! (see `system::noise`).
//!
//! Three layers, outermost first:
//!
//! - [`maintain_tunnel`] -- the supervisor. Resolves the portal, dials, and
//!   redials forever, so that exactly one tunnel is up whenever one can be.
//! - [`run_tunnel`] -- one connection, end to end: handshake, driver spawn,
//!   bootstrap, dispatch, return when it's over.
//! - [`run_bootstrap`] / [`run_dispatch_loop`] -- the two phases of a
//!   connection's life, in that order and never interleaved.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::{
    aggregator::{AppMessage, CommandResponse, CommandType},
    config::AppConfig,
    dusa_collection_utils::core::{
        errors::{ErrorArrayItem, Errors},
        logger::LogLevel,
    },
    identity::Identifier,
    network::resolve_url,
    portal::PortalMessage,
    state_persistence::AppState,
};
use colored::Colorize;
use simple_comms::network::driver::{
    ConnectionDriver, ConnectionHandle, ConnectionRole, DriverConfig, DriverMessage,
};
use simple_comms::network::send_receive::establish_connection_initiator;
use simple_comms::protocol::flags::{ConnectionParams, MsgType};
use simple_comms::protocol::message::ProtocolMessage;
use simple_comms::protocol::proto::Proto;
use tokio::net::TcpStream;
use tokio::time::{Duration, sleep};

use crate::network::command_processor;
use crate::system::tunnel_wire::{Correlated, TunnelMessage};

use super::{
    control::{Controls, PORTAL_CONTROLS, PortalState},
    manager::get_manager_data,
    ports::PORTAL_TUNNEL_PORT,
};

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct PortalAddr {
    addr: IpAddr,
    port: u32,
}

impl fmt::Display for PortalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let addr_colored = self.addr.to_string().blue(); // IP address in cyan
        let port_colored = self.port.to_string().purple(); // Port in yellow

        write!(f, "{}:{}", addr_colored, port_colored)
    }
}

pub async fn load_identifier() -> Option<Identifier> {
    match Identifier::load_from_file() {
        Ok(data) => Some(data),
        Err(_) => {
            log!(LogLevel::Warn, "System has no identity!");
            None
        }
    }
}

/// Resolves `portal.arhst.net` and files every address it produced into
/// [`PortalState`] as a dial candidate, each paired with the fleet port.
///
/// The hard-coded IP is the **DNS server** to ask, not the portal -- fleet
/// names are served by Artisan's own nameserver rather than whatever the
/// node's `/etc/resolv.conf` happens to point at, so a node with broken or
/// hijacked DNS still finds the right portal. The address differs by
/// environment because development resolves against the lab nameserver.
///
/// Resolution happens on every supervisor pass rather than once at startup, so
/// a portal that moves -- a failover, a re-IP -- is picked up by the next
/// redial instead of requiring a manager restart.
async fn get_portal_addr(config: &AppConfig) -> Result<(), ErrorArrayItem> {
    let portal_port: u32 = PORTAL_TUNNEL_PORT.into();

    let portal_addrs: Option<Vec<IpAddr>> = if config.environment == "development" {
        resolve_url(
            "portal.ah.internal", // FIXME portal.ah.internal is the new address we should use internally, update docs and other callsites
            Some(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 4))),
        )
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Network, err.to_string()))?
    } else {
        resolve_url(
            "portal.ah.internal",
            Some(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 4))),
        )
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Network, err.to_string()))?
    };

    if let Some(addrs) = portal_addrs {
        let mut portals: Vec<PortalAddr> = Vec::new();
        for ip in addrs {
            let portal: PortalAddr = PortalAddr {
                addr: ip,
                port: portal_port,
            };
            portals.push(portal);
        }

        PortalState::set_portal_addrs(PORTAL_CONTROLS.clone(), portals).await?;
        return Ok(());
    }

    Err(ErrorArrayItem::new(
        Errors::Network,
        "Failed to locate the portal".to_owned(),
    ))
}

/// Outer supervisor: keeps exactly one tunnel to the portal alive for the
/// life of the process, redialing whenever it ends. Intended to be spawned
/// once from `main.rs` and left running.
pub async fn maintain_tunnel(
    state: AppState,
    application_controls: Arc<Controls>,
    portal_pubkey: Arc<[u8; 32]>,
) {
    loop {
        if let Err(err) = get_portal_addr(&state.config).await {
            log!(LogLevel::Error, "Failed to resolve the portal's address: {}", err);
            sleep(Duration::from_secs(30)).await;
            continue;
        }

        let addrs = match PortalState::portal_addrs(PORTAL_CONTROLS.clone()).await {
            Ok(addrs) if !addrs.is_empty() => addrs,
            Ok(_) => {
                sleep(Duration::from_secs(30)).await;
                continue;
            }
            Err(err) => {
                log!(LogLevel::Error, "{}", err);
                sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        let mut ever_connected = false;
        for portal_addr in &addrs {
            match run_tunnel(
                portal_addr,
                &portal_pubkey,
                state.clone(),
                application_controls.clone(),
            )
            .await
            {
                Ok(()) => {
                    // Connected, ran for a while, and the peer eventually went
                    // away (Close, heartbeat timeout, or fatal I/O). Don't try
                    // the next candidate address -- this one is known-good --
                    // fall through and redial it from the top.
                    ever_connected = true;
                    break;
                }
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "Failed to establish a tunnel to portal @ {}: {}",
                        portal_addr,
                        err
                    );
                }
            }
        }

        // A tunnel that was up and then died is worth retrying quickly; one
        // that never connected at all gets the full backoff so we don't spin
        // on DNS/network trouble.
        sleep(Duration::from_secs(if ever_connected { 5 } else { 30 })).await;
    }
}

/// Establishes one tunnel connection to `portal_addr` and runs it to
/// completion: handshake, `ConnectionDriver` spawn, the one-time bootstrap,
/// then indefinite command dispatch. Returns once the connection has ended
/// for any reason -- `Ok(())` means it connected and later disconnected
/// (cleanly or not); `Err` means it never connected or never finished
/// bootstrapping.
async fn run_tunnel(
    portal_addr: &PortalAddr,
    portal_pubkey: &[u8; 32],
    mut state: AppState,
    application_controls: Arc<Controls>,
) -> Result<(), ErrorArrayItem> {
    let mut stream = TcpStream::connect(format!("{}:{}", portal_addr.addr, portal_addr.port))
        .await
        .map_err(ErrorArrayItem::from)?;

    // We are the initiator, so it is our job to declare the connection's
    // baseline. OPTIMIZED is compress + encrypt + encode + sign, deliberately
    // without INSECURE: we do not want the portal able to talk us down to
    // weaker params mid-connection via SIDEGRADE.
    let ctx = establish_connection_initiator(&mut stream, portal_pubkey, ConnectionParams::OPTIMIZED)
        .await?;

    let mut handle: ConnectionHandle<TunnelMessage> = ConnectionDriver::spawn(
        stream,
        ctx,
        ConnectionRole::Initiator {
            remote_static_pubkey: *portal_pubkey,
        },
        Proto::TCP,
        DriverConfig::default(),
    );

    log!(LogLevel::Debug, "Tunnel connected to portal @ {}", portal_addr);

    if let Err(err) = run_bootstrap(&mut handle, &mut state).await {
        let _ = handle.shutdown().await;
        return Err(err);
    }

    PortalState::portal_linked(PORTAL_CONTROLS.clone()).await?;
    log!(LogLevel::Info, "Registered with portal @ {}", portal_addr);

    run_dispatch_loop(&mut handle, &mut state, application_controls).await;

    Ok(())
}

/// The one-time bootstrap sequence at the start of a fresh tunnel connection:
/// Discover -> Id exchange -> Register. This replaces what used to be two
/// separate physical connections (`portal_discovery` then
/// `portal_registration`) with one sequential exchange over the one tunnel,
/// and drops the old `announce_node_key` step entirely -- the portal never
/// dials a node now, so there is no key left to hand it.
///
/// Unlike the old dance, there is no separate acknowledgement message for
/// handing off a freshly-issued identity: that `send_empty_ok` existed only
/// to satisfy `send_receive::send_message`'s blocking request/response
/// contract on the portal's old two-connection responder path, which the
/// full-duplex tunnel does not have.
async fn run_bootstrap(
    handle: &mut ConnectionHandle<TunnelMessage>,
    state: &mut AppState,
) -> Result<(), ErrorArrayItem> {
    send_bootstrap(handle, PortalMessage::Discover).await?;
    match recv_bootstrap(handle).await? {
        PortalMessage::IdRequest => {}
        other => return Err(unexpected_bootstrap(other)),
    }

    let local_id: Option<Identifier> = load_identifier().await;
    send_bootstrap(handle, PortalMessage::IdResponse(local_id)).await?;

    match recv_bootstrap(handle).await? {
        PortalMessage::IdResponse(Some(identifier)) => {
            // The portal issued us a fresh identity because we had none.
            adopt_identity(identifier).await?;
        }
        PortalMessage::IdResponse(None) => {
            // The portal already recognized the identity we sent; re-confirm
            // it against disk, mirroring the pre-tunnel behavior.
            adopt_identity(Identifier::load_from_file()?).await?;
        }
        PortalMessage::Error(err) => {
            return Err(ErrorArrayItem::new(Errors::ConnectionError, err));
        }
        other => return Err(unexpected_bootstrap(other)),
    }

    let manager_data = get_manager_data(state).await?;
    send_bootstrap(handle, PortalMessage::RegisterRequest(manager_data)).await?;

    match recv_bootstrap(handle).await? {
        PortalMessage::RegisterResponse(true) => Ok(()),
        PortalMessage::RegisterResponse(false) => Err(ErrorArrayItem::new(
            Errors::Network,
            "The portal declined our registration".to_owned(),
        )),
        PortalMessage::Error(err) => Err(ErrorArrayItem::new(
            Errors::Network,
            format!("Server responded: {}", err),
        )),
        other => Err(unexpected_bootstrap(other)),
    }
}

async fn adopt_identity(identifier: Identifier) -> Result<(), ErrorArrayItem> {
    if !identifier.verify().await {
        return Err(ErrorArrayItem::new(
            Errors::AuthenticationError,
            "Identifier verification failed".to_string(),
        ));
    }
    identifier.display_id();
    identifier.save_to_file()?;
    PortalState::set_identity(Some(identifier), PORTAL_CONTROLS.clone()).await
}

fn unexpected_bootstrap(message: PortalMessage) -> ErrorArrayItem {
    ErrorArrayItem::new(
        Errors::ConnectionError,
        format!("Unexpected message during bootstrap: {:?}", message),
    )
}

async fn send_bootstrap(
    handle: &ConnectionHandle<TunnelMessage>,
    message: PortalMessage,
) -> Result<(), ErrorArrayItem> {
    let framed = ProtocolMessage::new(
        ConnectionParams::OPTIMIZED,
        MsgType::Data,
        TunnelMessage::Bootstrap(message),
    )?;
    handle.send(framed).await
}

/// Waits for the next `Bootstrap` message. During this phase the portal has
/// no reason to send anything else, so anything else received here -- `App`
/// traffic, an `Open`/`OpenAck`, or the tunnel closing -- is itself an error.
async fn recv_bootstrap(
    handle: &mut ConnectionHandle<TunnelMessage>,
) -> Result<PortalMessage, ErrorArrayItem> {
    match handle.recv().await {
        Some(DriverMessage::Data(ProtocolMessage {
            payload: TunnelMessage::Bootstrap(message),
            ..
        })) => Ok(message),
        Some(DriverMessage::Data(ProtocolMessage {
            payload: TunnelMessage::App(_),
            ..
        })) => Err(ErrorArrayItem::new(
            Errors::ConnectionError,
            "Received command traffic before bootstrap completed".to_owned(),
        )),
        Some(_) => Err(ErrorArrayItem::new(
            Errors::ConnectionError,
            "Unexpected Open/OpenAck during bootstrap".to_owned(),
        )),
        None => Err(ErrorArrayItem::new(
            Errors::ConnectionError,
            "Tunnel closed during bootstrap".to_owned(),
        )),
    }
}

/// The steady-state loop for the rest of a tunnel connection's life: answer
/// whatever commands the portal pushes, using the same `command_processor`
/// the local debug listener uses (`crate::network`). Returns once
/// `handle.recv()` yields `None` -- the driver stopped (peer `Close`,
/// heartbeat timeout, or fatal I/O) -- leaving redialing to the caller.
async fn run_dispatch_loop(
    handle: &mut ConnectionHandle<TunnelMessage>,
    state: &mut AppState,
    application_controls: Arc<Controls>,
) {
    while let Some(msg) = handle.recv().await {
        let DriverMessage::Data(ProtocolMessage { payload, .. }) = msg else {
            log!(LogLevel::Warn, "Unexpected Open/OpenAck on the portal tunnel");
            continue;
        };

        let Correlated { request_id, body } = match payload {
            TunnelMessage::App(correlated) => correlated,
            TunnelMessage::Bootstrap(other) => {
                log!(
                    LogLevel::Warn,
                    "Unexpected bootstrap message after registration: {:?}",
                    other
                );
                continue;
            }
        };

        let response_body = match body {
            AppMessage::Command(command) => {
                let app_id = command.app_id.clone();
                let command_type = command.command_type.clone();

                match command_processor(command, application_controls.clone(), state).await {
                    Ok(data) => data,
                    Err(err) => AppMessage::Response(CommandResponse {
                        app_id,
                        command_type,
                        success: false,
                        message: Some(err.to_string()),
                    }),
                }
            }
            other => {
                log!(
                    LogLevel::Warn,
                    "Portal sent an illegal message over the tunnel: {:?}",
                    other
                );
                AppMessage::Response(CommandResponse {
                    app_id: "".into(),
                    command_type: CommandType::Custom("illegal message".into()),
                    success: false,
                    message: Some("Illegal message for this channel".into()),
                })
            }
        };

        let reply = match ProtocolMessage::new(
            ConnectionParams::OPTIMIZED,
            MsgType::Data,
            TunnelMessage::App(Correlated {
                request_id,
                body: response_body,
            }),
        ) {
            Ok(reply) => reply,
            Err(err) => {
                log!(LogLevel::Error, "Failed to frame a tunnel reply: {}", err);
                continue;
            }
        };

        if let Err(err) = handle.send(reply).await {
            log!(LogLevel::Error, "Failed to send a tunnel reply: {}", err);
            break;
        }
    }
}
