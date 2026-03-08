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
