//! A tiny operator CLI for the manager's **local, loopback-only** debug
//! listener (`127.0.0.1:9825`).
//!
//! This is not a portal. It speaks plain request/response
//! (`send_receive::send_message`) to `network::process_tcp`, one command per
//! connection, and only works from the same host as the manager. What makes
//! it useful is that the listener hands the command to the very same
//! `network::command_processor` the fleet tunnel uses, so what you see here is
//! what the portal would get -- minus the transport.
//!
//! Commanding a node *the way the portal does* means being the portal:
//! opening the fleet tunnel on `:9800`, running the bootstrap, and issuing
//! `Correlated<AppMessage>` over a `ConnectionDriver`. There is deliberately
//! no shortcut for that here. See `portal_upstream_api.md` §6.

use artisan_middleware::aggregator::{AppMessage, Command, CommandType};
use artisan_middleware::dusa_collection_utils::core::functions::current_timestamp;
use artisan_middleware::dusa_collection_utils::core::logger::{set_log_level, LogLevel};
use simple_comms::network::send_receive::{establish_connection_initiator, send_message};
use simple_comms::protocol::flags::ConnectionParams;
use simple_comms::protocol::message::ConnectionCtx;
use simple_comms::protocol::proto::Proto;
use std::net::SocketAddr;
use tokio::net::TcpStream;

// Each bin is its own crate root, so pull the key helpers in by path rather than
// duplicating them. `noise` has no intra-crate dependencies, which is what makes
// this work.
#[path = "../system/noise.rs"]
// The CLI only needs the public-key reader; the rest is for the daemon.
#[allow(dead_code)]
mod noise;

#[path = "../system/ports.rs"]
// Likewise: the CLI only dials the debug listener, but the module documents
// every port the manager touches and is worth keeping whole.
#[allow(dead_code)]
mod ports;

fn usage() -> &'static str {
    "\
ais_manager_debug (feature: debug-cli)

Talks to the manager's local, loopback-only debug listener on 127.0.0.1:9825
-- not the fleet tunnel the portal uses (the manager dials out to the portal
on :9800; nothing dials in). Only useful on the same host as the manager.

USAGE:
  ais_manager_debug [--addr HOST:PORT] [--pubkey HEX] [--insecure] <command> [args...]

COMMANDS:
  start <app>       Start an app (proxied to watchdog)
  stop <app>        Stop an app (proxied to watchdog; ais_manager triggers reload)
  restart <app>     Restart/reload an app (proxied to watchdog; ais_manager triggers reload)
  status <app>      Get status snapshot (returns JSON string)
  all-status        Get all status snapshots (returns JSON array string)
  info              Get ManagerData

FLAGS:
  --addr HOST:PORT  Default: 127.0.0.1:9825 (the debug listener; note this is
                    deliberately NOT the portal's :9800 fleet port, which this
                    tool cannot speak)
  --pubkey HEX      The target manager's Noise_NK public key (64 hex chars).
                    Defaults to the public= line of this host's
                    /opt/artisan/manager_identity.key, which is what you want
                    when talking to the manager on this machine.
  --insecure        Declare ConnectionParams::INSECURE, which disables the
                    version-in-band check and permits SIDEGRADE renegotiation
"
}

fn parse_addr(args: &mut Vec<String>) -> Result<(SocketAddr, bool, Option<String>), String> {
    let mut addr: SocketAddr = ports::DEBUG_LISTENER_ADDR
        .parse()
        .map_err(|e| format!("Default addr parse failed: {e}"))?;
    let mut insecure = false;
    let mut pubkey: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--addr" => {
                let Some(value) = args.get(i + 1) else {
                    return Err("--addr requires HOST:PORT".to_string());
                };
                addr = value
                    .parse()
                    .map_err(|e| format!("Invalid --addr '{value}': {e}"))?;
                args.drain(i..=i + 1);
            }
            "--pubkey" => {
                let Some(value) = args.get(i + 1) else {
                    return Err("--pubkey requires 64 hex characters".to_string());
                };
                pubkey = Some(value.clone());
                args.drain(i..=i + 1);
            }
            "--insecure" => {
                insecure = true;
                args.drain(i..=i);
            }
            _ => i += 1,
        }
    }

    Ok((addr, insecure, pubkey))
}

/// Resolves the target manager's `Noise_NK` public key: an explicit `--pubkey`
/// wins, otherwise fall back to the local manager's identity file.
fn resolve_pubkey(pubkey: Option<String>) -> Result<[u8; 32], String> {
    match pubkey {
        Some(hex_key) => {
            let bytes = hex::decode(hex_key.trim())
                .map_err(|e| format!("Invalid --pubkey hex: {e}"))?;
            bytes
                .try_into()
                .map_err(|_| "--pubkey must be exactly 32 bytes (64 hex characters)".to_string())
        }
        None => noise::read_local_public_key().map_err(|e| e.to_string()),
    }
}

fn parse_command(args: &[String]) -> Result<(CommandType, String), String> {
    let Some(cmd) = args.first() else {
        return Err("Missing <command>".to_string());
    };

    match cmd.as_str() {
        "start" | "stop" | "restart" | "status" => {
            let Some(app) = args.get(1) else {
                return Err(format!("{cmd} requires <app>"));
            };
            let ct = match cmd.as_str() {
                "start" => CommandType::Start,
                "stop" => CommandType::Stop,
                "restart" => CommandType::Restart,
                "status" => CommandType::Status,
                _ => unreachable!(),
            };
            Ok((ct, app.to_string()))
        }
        "all-status" => Ok((CommandType::AllStatus, "".to_string())),
        "info" => Ok((CommandType::Info, "".to_string())),
        other => Err(format!("Unknown command '{other}'")),
    }
}

async fn send_manager_command(
    stream: &mut TcpStream,
    pub_key: &[u8; 32],
    insecure: bool,
    app_id: String,
    command_type: CommandType,
) -> Result<AppMessage, String> {
    // The CLI dials the manager, so it plays the same initiator role the portal
    // does on this channel and declares the connection's baseline.
    let mut params: ConnectionParams = ConnectionParams::OPTIMIZED;
    if insecure {
        params |= ConnectionParams::INSECURE;
    }

    let mut conn: ConnectionCtx = establish_connection_initiator(stream, pub_key, params)
        .await
        .map_err(|e| {
            format!(
                "Noise_NK handshake with the manager failed: {e}. \
                 A wrong --pubkey looks exactly like this"
            )
        })?;

    let payload = AppMessage::Command(Command {
        app_id: app_id.into(),
        command_type,
        timestamp: current_timestamp(),
    });

    send_message::<TcpStream, AppMessage, AppMessage>(stream, payload, Proto::TCP, &mut conn)
        .await
        .map_err(|e| format!("send_message failed: {e}"))
}

#[tokio::main]
async fn main() -> Result<(), String> {
    set_log_level(LogLevel::Trace);
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", usage());
        return Ok(());
    }

    let (addr, insecure, pubkey) = parse_addr(&mut args)?;
    let (command_type, app) = parse_command(&args)?;
    let pub_key = resolve_pubkey(pubkey)?;

    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("Failed to connect to {addr}: {e}"))?;

    let response = send_manager_command(&mut stream, &pub_key, insecure, app, command_type).await?;
    match response {
        AppMessage::Response(r) => {
            if let Some(msg) = r.message {
                println!("{msg}");
            } else {
                println!(
                    "{{\"success\":{},\"app_id\":\"{}\",\"command_type\":\"{}\"}}",
                    r.success, r.app_id, r.command_type
                );
            }
        }
        AppMessage::ManagerInfo(info) => {
            println!("{info}");
        }
        other => {
            println!("{other}");
        }
    }

    Ok(())
}

