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
        "WatchdogListExpected" | "WatchdogGetConfigFile" | "WatchdogSetConfigFile" | "WatchdogRecalculateAllowedClients"
    )
}

pub fn map_config_file_kind(kind: &str) -> Option<i32> {
    match kind.to_lowercase().as_str() {
        "config" | "1" => Some(proto::ConfigFileKind::Config as i32),
        "overrides" | "2" => Some(proto::ConfigFileKind::Overrides as i32),
        _ => None,
    }
}

pub async fn handle(verb: &str, body: &str) -> AppMessage {
    match run(verb, body).await {
        Ok(json) => AppMessage::Response(CommandResponse {
            app_id: "".into(),
            command_type: CommandType::Custom(verb.to_owned()),
            success: true,
            message: Some(json),
        }),
        Err(err) => failure(verb, err.to_string()),
    }
}

fn failure(verb: &str, message: String) -> AppMessage {
    AppMessage::Response(CommandResponse {
        app_id: "".into(),
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
        other => Err(format!("Unknown watchdog config verb: {}", other)),
    }
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

pub async fn recalculate_allowed_clients() -> Result<proto::CommandResponse, ErrorArrayItem> {
    let mut client = watchdog_client().await?;
    let response = client
        .recalculate_allowed_clients(Request::new(proto::Empty {}))
        .await
        .map_err(|err| grpc_error(format!("Watchdog RecalculateAllowedClients failed: {}", err)))?
        .into_inner();

    Ok(response)
}
