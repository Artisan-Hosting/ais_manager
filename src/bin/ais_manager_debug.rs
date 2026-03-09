use artisan_middleware::aggregator::{AppMessage, Command, CommandType};
use artisan_middleware::dusa_collection_utils::core::functions::current_timestamp;
use artisan_middleware::dusa_collection_utils::core::logger::{set_log_level, LogLevel};
use simple_comms::network::send_receive::send_message;
use simple_comms::protocol::flags::Flags;
use simple_comms::protocol::proto::Proto;
use std::net::SocketAddr;
use tokio::net::TcpStream;

fn usage() -> &'static str {
    "\
ais_manager_debug (feature: debug-cli)

USAGE:
  ais_manager_debug [--addr HOST:PORT] [--insecure] <command> [args...]

COMMANDS:
  start <app>       Start an app (proxied to watchdog)
  stop <app>        Stop an app (proxied to watchdog; ais_manager triggers reload)
  restart <app>     Restart/reload an app (proxied to watchdog; ais_manager triggers reload)
  status <app>      Get status snapshot (returns JSON string)
  all-status        Get all status snapshots (returns JSON array string)
  info              Get ManagerData

FLAGS:
  --addr HOST:PORT  Default: 127.0.0.1:9800
  --insecure        Disable version-in-band check (simple_comms)
"
}

fn parse_addr(args: &mut Vec<String>) -> Result<(SocketAddr, bool), String> {
    let mut addr: SocketAddr = "127.0.0.1:9800"
        .parse()
        .map_err(|e| format!("Default addr parse failed: {e}"))?;
    let mut insecure = false;

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
            "--insecure" => {
                insecure = true;
                args.drain(i..=i);
            }
            _ => i += 1,
        }
    }

    Ok((addr, insecure))
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
    insecure: bool,
    app_id: String,
    command_type: CommandType,
) -> Result<AppMessage, String> {
    
    let payload = AppMessage::Command(Command {
        app_id: app_id.into(),
        command_type,
        timestamp: current_timestamp(),
    });

    let response = send_message::<TcpStream, AppMessage, AppMessage>(
        stream,
        Flags::OPTIMIZED,
        payload,
        Proto::TCP,
        insecure,
    )
    .await
    .map_err(|e| format!("send_message failed: {e}"))?;

    match response {
        Ok(message) => Ok(message.get_payload().await),
        Err(status) => Err(format!("Remote returned protocol status: {status}")),
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    set_log_level(LogLevel::Trace);
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", usage());
        return Ok(());
    }

    let (addr, insecure) = parse_addr(&mut args)?;
    let (command_type, app) = parse_command(&args)?;

    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("Failed to connect to {addr}: {e}"))?;

    let response = send_manager_command(&mut stream, insecure, app, command_type).await?;
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

