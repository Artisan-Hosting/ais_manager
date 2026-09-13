//! Client for `ais_secretserver`, used by the periodic per-app secrets sync
//! (Phase E, E10): pulls each locally-hosted app's full KV set and relays it
//! to watchdog to fold into that app's runtime bundle (via the existing
//! `SetConfigFile{kind: BUNDLE_ENV}` RPC, see `crate::watchdog::set_bundle_env`).
//!
//! Portal and the dashboard's "Secrets" page continue to read and write
//! these same secrets directly against `ais_secretserver`, unchanged -- this
//! client only ever reads, on Manager's own schedule, never writes.

pub mod proto {
    tonic::include_proto!("secret_service");
}

use artisan_middleware::dusa_collection_utils::core::errors::{ErrorArrayItem, Errors};
use proto::secret_service_client::SecretServiceClient;
use tonic::transport::Channel;

/// Fixed internal address for `ais_secretserver`, reachable from any node in
/// the fleet. `ah.internal` is resolved by our own FreeIPA server and this
/// traffic never leaves the internal network, which is the only reason
/// plaintext gRPC is acceptable here today.
///
/// FIXME(security): move this to TLS (or mTLS) once `ais_secretserver`
/// terminates it -- plain HTTP was a deliberate "internal network only, for
/// now" call, not a permanent one. Same gap exists in watchdog's own
/// `secrets::SECRET_SERVER_ADDR`; fix both together.
pub const SECRET_SERVER_ADDR: &str = "http://secrets.ah.internal:50052";

fn rpc_err(context: &str, status: tonic::Status) -> ErrorArrayItem {
    ErrorArrayItem::new(Errors::Network, format!("{context}: {status}"))
}

pub struct SecretClient {
    client: SecretServiceClient<Channel>,
}

impl SecretClient {
    pub async fn connect() -> Result<Self, ErrorArrayItem> {
        let client = SecretServiceClient::connect(SECRET_SERVER_ADDR)
            .await
            .map_err(|err| {
                ErrorArrayItem::new(
                    Errors::Network,
                    format!("Connecting to secret-server at {SECRET_SERVER_ADDR}: {err}"),
                )
            })?;
        Ok(Self { client })
    }

    /// Fetches every secret currently stored for `runner_id`/`environment_id`,
    /// formatted as `KEY=value\n` lines ready to commit into a runtime
    /// bundle's env content. A value that isn't valid UTF-8 is skipped
    /// (logged by the caller) rather than failing the whole sync.
    pub async fn get_all_as_env_lines(
        &mut self,
        runner_id: &str,
        environment_id: &str,
    ) -> Result<String, ErrorArrayItem> {
        let request = proto::GetAllSecretsRequest {
            runner_id: runner_id.to_owned(),
            environment_id: environment_id.to_owned(),
            version: 0,
        };

        let response = self
            .client
            .get_all_secrets(request)
            .await
            .map_err(|status| rpc_err("get_all_secrets", status))?
            .into_inner();

        let mut lines = String::new();
        for kv in response.vals {
            match String::from_utf8(kv.value) {
                Ok(value) => lines.push_str(&format!("{}={}\n", kv.key, value)),
                Err(_) => continue,
            }
        }
        Ok(lines)
    }
}
