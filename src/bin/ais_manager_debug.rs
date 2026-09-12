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
  ais_manager_debug [--addr HOST:PORT] [--pubkey HEX] [--insecure] [--no-reload] <command> [args...]

COMMANDS:
  start <app>       Start an app (proxied to watchdog)
  stop <app>        Stop an app (proxied to watchdog; ais_manager triggers reload)
  restart <app>     Restart/reload an app (proxied to watchdog; ais_manager triggers reload)
  status <app>      Get status snapshot (returns JSON string)
  all-status        Get all status snapshots (returns JSON array string)
  info              Get ManagerData
  git-repos get                     Dump the git monitor's repo list (this is
                                    also the backup format)
  git-repos set <file.json>         Replace the whole list from a dump
  git-repos add <file.json>         Add one repo
  git-repos update <id> <file.json> Replace the repo with that id
  git-repos remove <id>             Delete the repo with that id
  list-expected                     List expected apps from watchdog
  get-config <app> <config|overrides> [create_if_missing]
                                    Get config/override file contents
  set-config <app> <config|overrides> <file.toml> [expected_previous_sha256]
                                    Set config/override file from local file

  Mutating git-repos commands restart ais_gitmon so the change takes effect.
  Pass --no-reload to skip that when batching several edits; restart once at
  the end by re-running the last edit without it.

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
  --no-reload       git-repos only: write the file but do not restart
                    ais_gitmon. The edit does not take effect until it is
                    restarted.
"
}

fn parse_addr(args: &mut Vec<String>) -> Result<(SocketAddr, bool, bool, Option<String>), String> {
    let mut addr: SocketAddr = ports::DEBUG_LISTENER_ADDR
        .parse()
        .map_err(|e| format!("Default addr parse failed: {e}"))?;
    let mut insecure = false;
    let mut reload = true;
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
            "--no-reload" => {
                reload = false;
                args.drain(i..=i);
            }
            _ => i += 1,
        }
    }

    Ok((addr, insecure, reload, pubkey))
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

fn parse_command(args: &[String], reload: bool) -> Result<(CommandType, String), String> {
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
        "list-expected" => {
            Ok((CommandType::Custom("WatchdogListExpected".to_string()), "".to_string()))
        }
        "get-config" => {
            let Some(app) = args.get(1) else {
                return Err("get-config requires <app>".to_string());
            };
            let Some(kind) = args.get(2) else {
                return Err("get-config requires <config|overrides>".to_string());
            };
            let create_if_missing = args.get(3).map(|s| s == "true" || s == "create_if_missing").unwrap_or(false);

            let payload = serde_json::json!({
                "application": app,
                "kind": kind,
                "create_if_missing": create_if_missing,
            });
            Ok((CommandType::Custom(format!("WatchdogGetConfigFile {payload}")), "".to_string()))
        }
        "set-config" => {
            let Some(app) = args.get(1) else {
                return Err("set-config requires <app>".to_string());
            };
            let Some(kind) = args.get(2) else {
                return Err("set-config requires <config|overrides>".to_string());
            };
            let Some(file_path) = args.get(3) else {
                return Err("set-config requires <file.toml>".to_string());
            };
            let expected_previous_sha256 = args.get(4).cloned().unwrap_or_default();

            let content = std::fs::read_to_string(file_path)
                .map_err(|err| format!("Reading {file_path}: {err}"))?;

            let payload = serde_json::json!({
                "application": app,
                "kind": kind,
                "content": content,
                "expected_previous_sha256": expected_previous_sha256,
            });
            Ok((CommandType::Custom(format!("WatchdogSetConfigFile {payload}")), "".to_string()))
        }
        // The manager reads these as `Custom("<Verb> <json>")` -- the JSON body
        // has to ride inside the verb string because `Command` has no field for
        // one. `app_id` is unused for them, hence the empty string.
        "git-repos" => parse_git_repos(&args[1..], reload).map(|ct| (ct, String::new())),
        other => Err(format!("Unknown command '{other}'")),
    }
}

/// Builds the `Custom("<Verb> <json>")` string for a `git-repos` subcommand.
///
/// Bodies come from files rather than argv: a repo entry is a JSON object, and
/// shell-quoting one by hand is how you end up debugging a quoting problem
/// instead of the thing you were actually testing.
fn parse_git_repos(args: &[String], reload: bool) -> Result<CommandType, String> {
    let Some(sub) = args.first() else {
        return Err("git-repos requires <get|set|add|update|remove>".to_string());
    };

    let read_body = |path: &String| -> Result<serde_json::Value, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|err| format!("Reading {path}: {err}"))?;
        serde_json::from_str(&raw).map_err(|err| format!("{path} is not valid JSON: {err}"))
    };

    // `reload` is merged into the body object rather than sent separately,
    // since the whole request is one JSON document to the manager.
    let with_reload = |mut value: serde_json::Value| -> Result<String, String> {
        match value.as_object_mut() {
            Some(map) => {
                map.insert("reload".to_string(), serde_json::Value::Bool(reload));
                Ok(value.to_string())
            }
            None => Err("Body must be a JSON object".to_string()),
        }
    };

    let payload = match sub.as_str() {
        "get" => return Ok(CommandType::Custom("GitReposGet".to_string())),

        "set" | "add" => {
            let Some(file) = args.get(1) else {
                return Err(format!("git-repos {sub} requires <file.json>"));
            };
            let verb = if sub == "set" { "GitReposSet" } else { "GitReposAdd" };
            format!("{verb} {}", with_reload(read_body(file)?)?)
        }

        "update" => {
            let (Some(id), Some(file)) = (args.get(1), args.get(2)) else {
                return Err("git-repos update requires <id> <file.json>".to_string());
            };
            let body = serde_json::json!({ "id": id, "repo": read_body(file)?, "reload": reload });
            format!("GitReposUpdate {body}")
        }

        "remove" => {
            let Some(id) = args.get(1) else {
                return Err("git-repos remove requires <id>".to_string());
            };
            let body = serde_json::json!({ "id": id, "reload": reload });
            format!("GitReposRemove {body}")
        }

        other => return Err(format!("Unknown git-repos subcommand '{other}'")),
    };

    Ok(CommandType::Custom(payload))
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

    let (addr, insecure, reload, pubkey) = parse_addr(&mut args)?;
    let (command_type, app) = parse_command(&args, reload)?;
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


#[cfg(test)]
mod tests {
    use super::*;

    fn body_file(name: &str, contents: &str) -> String {
        let path = std::env::temp_dir().join(format!("ais_dbg_{}_{}.json", name, std::process::id()));
        std::fs::write(&path, contents).unwrap();
        path.display().to_string()
    }

    fn custom(args: &[&str], reload: bool) -> String {
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        match parse_git_repos(&owned, reload).unwrap() {
            CommandType::Custom(raw) => raw,
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    /// The manager splits this string on the first space and parses the rest as
    /// JSON, so what the CLI emits has to survive exactly that.
    #[test]
    fn emits_verb_then_json_body() {
        let file = body_file("add", r#"{"user":"acme","repo":"web","branch":"main","server":"GitHub"}"#);

        let raw = custom(&["add", &file], true);
        let (verb, body) = raw.split_once(' ').unwrap();
        assert_eq!(verb, "GitReposAdd");

        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["user"], "acme");
        assert_eq!(parsed["reload"], true, "--no-reload defaults off");

        // The flag has to reach the manager, not just the CLI's own state.
        let raw = custom(&["add", &file], false);
        let body: serde_json::Value = serde_json::from_str(raw.split_once(' ').unwrap().1).unwrap();
        assert_eq!(body["reload"], false);
    }

    #[test]
    fn get_has_no_body_to_split_on() {
        assert_eq!(custom(&["get"], true), "GitReposGet");
    }

    #[test]
    fn update_wraps_the_file_under_repo_with_its_id() {
        let file = body_file("upd", r#"{"user":"acme","repo":"web","branch":"dev","server":"GitHub"}"#);
        let raw = custom(&["update", "a1b2c3d4", &file], true);

        let (verb, body) = raw.split_once(' ').unwrap();
        assert_eq!(verb, "GitReposUpdate");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["id"], "a1b2c3d4");
        assert_eq!(parsed["repo"]["branch"], "dev");
    }

    #[test]
    fn remove_needs_only_an_id() {
        let raw = custom(&["remove", "a1b2c3d4"], true);
        let (verb, body) = raw.split_once(' ').unwrap();
        assert_eq!(verb, "GitReposRemove");
        assert_eq!(serde_json::from_str::<serde_json::Value>(body).unwrap()["id"], "a1b2c3d4");
    }

    /// A repo can legitimately carry spaces (a custom server URL), and the
    /// manager splits on the first space only -- so the body must survive.
    #[test]
    fn a_body_containing_spaces_survives_the_split() {
        let file = body_file(
            "spaces",
            r#"{"user":"acme","repo":"web","branch":"main","server":{"Custom":"https://git example.com/"}}"#,
        );
        let raw = custom(&["add", &file], true);

        let body = raw.split_once(' ').unwrap().1;
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["server"]["Custom"], "https://git example.com/");
    }

    #[test]
    fn a_non_object_body_is_refused_before_it_reaches_the_wire() {
        let file = body_file("array", r#"[1,2,3]"#);
        assert!(parse_git_repos(&["add".to_string(), file], true).is_err());
    }
}
