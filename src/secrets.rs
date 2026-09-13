//! Client for `ais_secretserver`, used by the periodic per-app secrets sync
//! (Phase E, E10): pulls each locally-hosted app's full KV set and relays it
//! to watchdog to fold into that app's runtime bundle (via the existing
//! `SetConfigFile{kind: BUNDLE_ENV}` RPC, see `crate::watchdog::set_bundle_env`).
//!
//! Portal and the dashboard's "Secrets" page remain the read/write boundary
//! humans use against `ais_secretserver`, unchanged. This client's own writes
//! (`seed_from_env_lines`) are narrower: they only ever fill a confirmed gap
//! (an app with real content in its bundle but nothing on secret-server yet),
//! never overwrite an existing record.

pub mod proto {
    tonic::include_proto!("secret_service");
}

use artisan_middleware::dusa_collection_utils::core::{
    errors::{ErrorArrayItem, Errors},
    logger::LogLevel,
};
use artisan_middleware::dusa_collection_utils::log;
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

    /// Creates one arbitrary-app secret. Returns `Ok(false)` rather than an
    /// error when the write is rejected -- `ais_secretserver`'s
    /// `create_secret` handler collapses every DB error (a genuine outage as
    /// much as a duplicate-key conflict from a key that's already there)
    /// into `SimpleSecretResponse { success: false }` with no distinguishing
    /// text, so a "false" here is treated as "already present, nothing to
    /// do" by the only caller (`seed_from_env_lines`), not as fatal.
    async fn create_app_secret(
        &mut self,
        runner_id: &str,
        environment_id: &str,
        secret_key: &str,
        value: &str,
    ) -> Result<bool, ErrorArrayItem> {
        let request = proto::CreateSecretRequest {
            runner_id: runner_id.to_owned(),
            environment_id: environment_id.to_owned(),
            secret_key: secret_key.to_owned(),
            value: value.to_owned(),
            actor: "ais_manager".to_owned(),
        };

        let response = self
            .client
            .create_secret(request)
            .await
            .map_err(|status| rpc_err("create_secret", status))?
            .into_inner();

        Ok(response.success)
    }

    /// Seeds secret-server with `env_content`'s `KEY=value` pairs for
    /// `runner_id`/`environment_id`, one `CreateSecret` call per key.
    ///
    /// Backfills the case watchdog's own migration-time seed (see
    /// `runtime_bundle_lifecycle::migrate_app_to_bundle` on the watchdog
    /// side) can't reach: an app whose bundle was already built *before*
    /// that seeding existed, so it never ran. This periodic sync loop
    /// already round-trips every locally-hosted app -- when secret-server
    /// comes back empty but the bundle it would otherwise overwrite already
    /// has real content, that content is pushed up here instead, so it
    /// becomes the fleet-wide record other instances can pull down. Called
    /// only when secret-server was just confirmed to have nothing for this
    /// app -- never used to overwrite an existing record, only to fill a gap
    /// the first time it's found.
    pub async fn seed_from_env_lines(
        &mut self,
        runner_id: &str,
        environment_id: &str,
        env_content: &str,
    ) -> Result<(), ErrorArrayItem> {
        for line in env_content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key.is_empty() {
                continue;
            }

            match self.create_app_secret(runner_id, environment_id, key, value).await {
                Ok(true) => {}
                Ok(false) => {
                    log!(
                        LogLevel::Debug,
                        "Seeding secret-server: '{}' already has a value for {}/{} (likely raced with another instance); leaving it as-is",
                        key,
                        runner_id,
                        environment_id
                    );
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}
