use artisan_middleware::dusa_collection_utils::core::errors::{ErrorArrayItem, Errors};
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tonic::{
    transport::{Channel, Endpoint, Uri},
    Request,
};
use tower::service_fn;
use artisan_middleware::aggregator::{AppMessage, CommandResponse, CommandType};
use serde::{Deserialize, Serialize};

const WATCHDOG_SOCKET_PATH: &str = "/tmp/artisan_watchdog.sock";

/// Generated protobuf/gRPC bindings for `watchdog.proto`.
pub mod proto {
    tonic::include_proto!("artisan.watchdog");
}

fn grpc_error<T: ToString>(message: T) -> ErrorArrayItem {
    ErrorArrayItem::new(Errors::Network, message.to_string())
}

async fn watchdog_client() -> Result<proto::watchdog_client::WatchdogClient<Channel>, ErrorArrayItem>
{
    let endpoint = Endpoint::try_from("http://[::]:50051")
        .map_err(|err| grpc_error(format!("Failed to initialize watchdog endpoint: {}", err)))?;

    let socket_path = WATCHDOG_SOCKET_PATH.to_string();
    let channel = endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket_path = socket_path.clone();
            async move {
                let stream = UnixStream::connect(socket_path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .map_err(|err| grpc_error(format!("Failed to connect to watchdog socket: {}", err)))?;

    Ok(proto::watchdog_client::WatchdogClient::new(channel))
}

pub fn socket_path() -> &'static str {
    WATCHDOG_SOCKET_PATH
}

pub async fn is_reachable(timeout_duration: Duration) -> bool {
    let attempt = timeout(timeout_duration, async {
        let mut client = watchdog_client().await?;
        client
            .get_version_info(Request::new(proto::Empty {}))
            .await
            .map_err(|err| grpc_error(format!("Watchdog GetVersionInfo failed: {}", err)))?;
        Ok::<(), ErrorArrayItem>(())
    })
    .await;

    match attempt {
        Ok(Ok(())) => true,
        _ => false,
    }
}

pub async fn execute_start(application: &str) -> Result<proto::CommandResponse, ErrorArrayItem> {
    execute_command(proto::command_request::Payload::Start(
        proto::StartCommand {
            application: application.to_owned(),
        },
    ))
    .await
}

pub async fn execute_stop(application: &str) -> Result<proto::CommandResponse, ErrorArrayItem> {
    execute_command(proto::command_request::Payload::Stop(proto::StopCommand {
        application: application.to_owned(),
    }))
    .await
}

pub async fn execute_reload(application: &str) -> Result<proto::CommandResponse, ErrorArrayItem> {
    execute_command(proto::command_request::Payload::Reload(
        proto::ReloadCommand {
            application: application.to_owned(),
        },
    ))
    .await
}

pub async fn list_applications() -> Result<Vec<proto::ApplicationStatusMessage>, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let response = client
        .list_applications(Request::new(proto::Empty {}))
        .await
        .map_err(|err| grpc_error(format!("Watchdog ListApplications failed: {}", err)))?
        .into_inner();

    Ok(response.applications)
}

pub async fn get_security_trip_status() -> Result<proto::SecurityTripStatus, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let response = client
        .get_security_trip_status(Request::new(proto::Empty {}))
        .await
        .map_err(|err| grpc_error(format!("Watchdog GetSecurityTripStatus failed: {}", err)))?
        .into_inner();

    Ok(response)
}

async fn execute_command(
    payload: proto::command_request::Payload,
) -> Result<proto::CommandResponse, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let request = proto::CommandRequest {
        payload: Some(payload),
    };

    let response = client
        .execute_command(Request::new(request))
        .await
        .map_err(|err| grpc_error(format!("Watchdog ExecuteCommand failed: {}", err)))?
        .into_inner();

    Ok(response)
}

#[derive(Deserialize)]
pub struct GetConfigFileJson {
    pub application: String,
    pub kind: String,
    #[serde(default)]
    pub create_if_missing: bool,
}

#[derive(Serialize)]
pub struct GetConfigFileJsonResponse {
    pub found: bool,
    pub created: bool,
    pub path: String,
    pub content: String,
    pub sha256: String,
}

#[derive(Deserialize)]
pub struct SetConfigFileJson {
    pub application: String,
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub expected_previous_sha256: String,
}

#[derive(Serialize)]
pub struct SetConfigFileJsonResponse {
    pub accepted: bool,
    pub message: String,
    pub backup_file: String,
}

#[derive(Serialize)]
pub struct ExpectedAppsJson {
    pub expected: Vec<String>,
    pub safe: Vec<String>,
    pub last_scan: u64,
}

pub fn handles(verb: &str) -> bool {
    matches!(
        verb,
        "WatchdogListExpected"
            | "WatchdogGetConfigFile"
            | "WatchdogSetConfigFile"
            | "WatchdogRecalculateAllowedClients"
            | "WatchdogListBuilds"
            | "WatchdogGetApplication"
    )
}

pub fn map_config_file_kind(kind: &str) -> Option<i32> {
    match kind.to_lowercase().as_str() {
        "config" | "1" => Some(proto::ConfigFileKind::Config as i32),
        "overrides" | "2" => Some(proto::ConfigFileKind::Overrides as i32),
        // Phase E, E11: the two bundle-backed structural-config kinds. Not
        // yet wired up on the Portal side, but harmless to accept here.
        "runtime" | "3" => Some(proto::ConfigFileKind::Runtime as i32),
        "custom" | "4" => Some(proto::ConfigFileKind::Custom as i32),
        _ => None,
    }
}

pub async fn handle(verb: &str, body: &str) -> AppMessage {
    match run(verb, body).await {
        Ok(json) => AppMessage::Response(CommandResponse {
            project_id: "".into(),
            command_type: CommandType::Custom(verb.to_owned()),
            success: true,
            message: Some(json),
        }),
        Err(err) => failure(verb, err.to_string()),
    }
}

fn failure(verb: &str, message: String) -> AppMessage {
    AppMessage::Response(CommandResponse {
        project_id: "".into(),
        command_type: CommandType::Custom(verb.to_owned()),
        success: false,
        message: Some(message),
    })
}

async fn run(verb: &str, body: &str) -> Result<String, String> {
    match verb {
        "WatchdogRecalculateAllowedClients" => {
            let res = recalculate_allowed_clients().await
                .map_err(|err| err.err_mesg.to_string())?;
            let json = serde_json::json!({
                "accepted": res.accepted,
                "message": res.message,
            });
            serde_json::to_string(&json).map_err(|err| format!("JSON serialization error: {}", err))
        }
        "WatchdogListExpected" => {
            let res = list_expected_apps().await
                .map_err(|err| err.err_mesg.to_string())?;
            let json = ExpectedAppsJson {
                expected: res.expected,
                safe: res.safe,
                last_scan: res.last_scan,
            };
            serde_json::to_string(&json).map_err(|err| format!("JSON serialization error: {}", err))
        }
        "WatchdogGetConfigFile" => {
            let req: GetConfigFileJson = if body.trim().is_empty() {
                return Err("Missing JSON body for WatchdogGetConfigFile".to_string());
            } else {
                serde_json::from_str(body).map_err(|err| format!("Invalid JSON: {}", err))?
            };

            let kind = map_config_file_kind(&req.kind)
                .ok_or_else(|| format!("Invalid kind '{}'. Must be 'config' or 'overrides'", req.kind))?;

            let res = get_config_file(&req.application, kind, req.create_if_missing).await
                .map_err(|err| err.err_mesg.to_string())?;

            let json = GetConfigFileJsonResponse {
                found: res.found,
                created: res.created,
                path: res.path,
                content: res.content,
                sha256: res.sha256,
            };
            serde_json::to_string(&json).map_err(|err| format!("JSON serialization error: {}", err))
        }
        "WatchdogSetConfigFile" => {
            let req: SetConfigFileJson = if body.trim().is_empty() {
                return Err("Missing JSON body for WatchdogSetConfigFile".to_string());
            } else {
                serde_json::from_str(body).map_err(|err| format!("Invalid JSON: {}", err))?
            };

            let kind = map_config_file_kind(&req.kind)
                .ok_or_else(|| format!("Invalid kind '{}'. Must be 'config' or 'overrides'", req.kind))?;

            let res = set_config_file(&req.application, kind, &req.content, &req.expected_previous_sha256).await
                .map_err(|err| err.err_mesg.to_string())?;

            let json = SetConfigFileJsonResponse {
                accepted: res.accepted,
                message: res.message,
                backup_file: res.backup_file,
            };
            serde_json::to_string(&json).map_err(|err| format!("JSON serialization error: {}", err))
        }
        "WatchdogListBuilds" => {
            let builds = list_builds().await.map_err(|err| err.err_mesg.to_string())?;
            serde_json::to_string(&builds_json(builds)).map_err(|err| format!("JSON serialization error: {}", err))
        }
        "WatchdogGetApplication" => {
            let req: GetApplicationJson = if body.trim().is_empty() {
                return Err("Missing JSON body for WatchdogGetApplication".to_string());
            } else {
                serde_json::from_str(body).map_err(|err| format!("Invalid JSON: {}", err))?
            };
            let res = get_application(&req.application).await.map_err(|err| err.err_mesg.to_string())?;
            serde_json::to_string(&application_json(&req.application, res, LOG_TAIL_LINES))
                .map_err(|err| format!("JSON serialization error: {}", err))
        }
        other => Err(format!("Unknown watchdog config verb: {}", other)),
    }
}

/// How many trailing stdout/stderr lines `WatchdogGetApplication` returns: enough
/// to show why a build or start failed, without shipping the whole rolling buffer.
const LOG_TAIL_LINES: usize = 20;

#[derive(Deserialize)]
pub struct GetApplicationJson {
    pub application: String,
}

/// `BUILD_RESULT_SUCCESS` -> `success`. Anything this build does not recognise is
/// `unknown` rather than guessed at.
fn build_result_name(code: i32) -> &'static str {
    match proto::BuildResult::try_from(code) {
        Ok(proto::BuildResult::Success) => "success",
        Ok(proto::BuildResult::Failure) => "failure",
        _ => "unknown",
    }
}

#[derive(Serialize)]
pub struct BuildJson {
    pub name: String,
    pub result: &'static str,
    pub timestamp: u64,
    pub vetted: bool,
}

#[derive(Serialize)]
pub struct BuildsJson {
    pub builds: Vec<BuildJson>,
}

pub fn builds_json(list: proto::BuildStatusList) -> BuildsJson {
    BuildsJson {
        builds: list
            .builds
            .into_iter()
            .map(|b| BuildJson { name: b.name, result: build_result_name(b.result), timestamp: b.timestamp, vetted: b.vetted })
            .collect(),
    }
}

#[derive(Serialize)]
pub struct ApplicationJson {
    pub found: bool,
    pub name: String,
    pub status: String,
    pub pid: Option<u32>,
    pub last_updated: u64,
    /// 0 when the app has no allocated port.
    pub port: u32,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

fn tail(entries: &[proto::StdLogEntry], n: usize) -> Vec<String> {
    let start = entries.len().saturating_sub(n);
    entries[start..].iter().map(|e| e.line.clone()).collect()
}

pub fn application_json(name: &str, res: proto::ApplicationStatusResponse, tail_lines: usize) -> ApplicationJson {
    match (res.found, res.status) {
        (true, Some(s)) => ApplicationJson {
            found: true,
            name: s.name,
            status: s.status,
            pid: s.pid,
            last_updated: s.last_updated,
            port: s.port,
            stdout: tail(&s.stdout, tail_lines),
            stderr: tail(&s.stderr, tail_lines),
        },
        _ => ApplicationJson {
            found: false,
            name: name.to_owned(),
            status: String::new(),
            pid: None,
            last_updated: 0,
            port: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    }
}

pub async fn list_builds() -> Result<proto::BuildStatusList, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    Ok(client
        .list_builds(Request::new(proto::Empty {}))
        .await
        .map_err(|err| grpc_error(format!("Watchdog ListBuilds failed: {}", err)))?
        .into_inner())
}

pub async fn get_application(application: &str) -> Result<proto::ApplicationStatusResponse, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    Ok(client
        .get_application(Request::new(proto::ApplicationStatusRequest { name: application.to_owned() }))
        .await
        .map_err(|err| grpc_error(format!("Watchdog GetApplication failed: {}", err)))?
        .into_inner())
}

pub async fn list_expected_apps() -> Result<proto::ExpectedAppsList, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let response = client
        .list_expected_apps(Request::new(proto::Empty {}))
        .await
        .map_err(|err| grpc_error(format!("Watchdog ListExpectedApps failed: {}", err)))?
        .into_inner();

    Ok(response)
}

pub async fn get_config_file(
    application: &str,
    kind: i32,
    create_if_missing: bool,
) -> Result<proto::GetConfigFileResponse, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let request = proto::GetConfigFileRequest {
        application: application.to_owned(),
        kind,
        create_if_missing,
    };
    let response = client
        .get_config_file(Request::new(request))
        .await
        .map_err(|err| grpc_error(format!("Watchdog GetConfigFile failed: {}", err)))?
        .into_inner();

    Ok(response)
}

pub async fn set_config_file(
    application: &str,
    kind: i32,
    content: &str,
    expected_previous_sha256: &str,
) -> Result<proto::SetConfigFileResponse, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let request = proto::SetConfigFileRequest {
        application: application.to_owned(),
        kind,
        content: content.to_owned(),
        expected_previous_sha256: expected_previous_sha256.to_owned(),
    };
    let response = client
        .set_config_file(Request::new(request))
        .await
        .map_err(|err| grpc_error(format!("Watchdog SetConfigFile failed: {}", err)))?
        .into_inner();

    Ok(response)
}

/// Pushes secret-server's current KV set for `application` down into its
/// runtime bundle's env content (Phase E, E10). Manager is the authoritative
/// source being relayed here, not a competing edit that needs reconciling
/// against watchdog's own copy, so this always writes with an empty
/// `expected_previous_sha256` -- see `try_write_bundle_kind`'s doc comment on
/// the watchdog side for the matching reasoning.
pub async fn set_bundle_env(application: &str, content: &str) -> Result<proto::SetConfigFileResponse, ErrorArrayItem> {
    set_config_file(application, proto::ConfigFileKind::BundleEnv as i32, content, "").await
}

/// Reads back the `environment` field of an app's fixed runtime config, so
/// the secrets-sync loop knows which secret-server `environment_id` to pull
/// (Phase E, E10). Returns `Ok(None)` when the app has no runtime bundle yet
/// (nothing to sync until it's created) rather than treating that as an
/// error.
pub async fn get_app_environment(application: &str) -> Result<Option<String>, ErrorArrayItem> {
    let res = get_config_file(application, proto::ConfigFileKind::Runtime as i32, false).await?;
    if !res.found || res.content.trim().is_empty() {
        return Ok(None);
    }

    #[derive(Deserialize)]
    struct RuntimeEnvironmentOnly {
        environment: String,
    }

    let parsed: RuntimeEnvironmentOnly = toml::from_str(&res.content).map_err(|err| {
        ErrorArrayItem::new(
            Errors::GeneralError,
            format!("Parsing runtime config for '{}' to read environment field: {}", application, err),
        )
    })?;

    Ok(Some(parsed.environment))
}

/// Reads back an app's bundle env content as watchdog currently has it,
/// without going through secret-server at all. Used by the secrets-sync loop
/// to backfill secret-server when it comes back empty for an app whose
/// bundle was already built (and possibly already seeded from a legacy local
/// `.env`) before that seed-on-migration path existed. `Ok(None)` covers both
/// "no bundle yet" and "bundle exists but its env content is empty" -- either
/// way, there's nothing here worth pushing up.
pub async fn get_bundle_env(application: &str) -> Result<Option<String>, ErrorArrayItem> {
    let res = get_config_file(application, proto::ConfigFileKind::BundleEnv as i32, false).await?;
    if !res.found || res.content.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(res.content))
}

pub async fn recalculate_allowed_clients() -> Result<proto::CommandResponse, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let response = client
        .recalculate_allowed_clients(Request::new(proto::Empty {}))
        .await
        .map_err(|err| grpc_error(format!("Watchdog RecalculateAllowedClients failed: {}", err)))?
        .into_inner();

    Ok(response)
}

#[cfg(test)]
mod deploy_status_tests {
    use super::*;

    fn line(n: usize) -> proto::StdLogEntry {
        proto::StdLogEntry { timestamp: n as u64, line: format!("line {n}") }
    }

    #[test]
    fn the_new_verbs_are_recognised() {
        assert!(handles("WatchdogListBuilds"));
        assert!(handles("WatchdogGetApplication"));
        assert!(!handles("WatchdogNope"));
    }

    #[test]
    fn builds_are_reported_with_words_not_enum_numbers() {
        let list = proto::BuildStatusList {
            builds: vec![
                proto::BuildStatusMessage { name: "ais_a".into(), result: proto::BuildResult::Success as i32, timestamp: 5, vetted: true },
                proto::BuildStatusMessage { name: "ais_b".into(), result: proto::BuildResult::Failure as i32, timestamp: 6, vetted: false },
                proto::BuildStatusMessage { name: "ais_c".into(), result: 99, timestamp: 7, vetted: false },
            ],
        };
        let json = builds_json(list);
        let results: Vec<_> = json.builds.iter().map(|b| (b.name.as_str(), b.result)).collect();
        assert_eq!(results, [("ais_a", "success"), ("ais_b", "failure"), ("ais_c", "unknown")]);
    }

    #[test]
    fn an_application_carries_its_port_and_only_the_log_tail() {
        let status = proto::ApplicationStatusMessage {
            name: "ais_a".into(),
            status: "Running".into(),
            pid: Some(42),
            last_updated: 9,
            port: 20007,
            stdout: (0..50).map(line).collect(),
            stderr: vec![line(1)],
            ..Default::default()
        };
        let json = application_json("ais_a", proto::ApplicationStatusResponse { found: true, status: Some(status) }, 20);
        assert!(json.found);
        assert_eq!((json.status.as_str(), json.port, json.pid), ("Running", 20007, Some(42)));
        assert_eq!(json.stdout.len(), 20);
        assert_eq!(json.stdout.first().map(String::as_str), Some("line 30"), "the LAST 20 lines");
        assert_eq!(json.stderr, ["line 1"]);
    }

    #[test]
    fn an_unknown_application_is_not_found_not_an_error() {
        let json = application_json("ais_zzz", proto::ApplicationStatusResponse { found: false, status: None }, 20);
        assert!(!json.found);
        assert_eq!(json.name, "ais_zzz");
        assert_eq!(json.port, 0);
    }
}
