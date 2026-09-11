//! The wire envelope carried over the manager's single full-duplex tunnel to
//! the portal. See `simple_comms`' `docs/HANDSHAKE.md` (`ConnectionDriver`
//! section), `Manager/portal_upstream_api.md` §2/§2.1, and
//! `portal/docs/fleet_tunnel.md` §8.
//!
//! `ConnectionDriver<APP>` is monomorphic over one payload type per
//! connection, but this tunnel carries two logically distinct kinds of
//! traffic on the same connection: a one-time registration bootstrap
//! (`PortalMessage`) followed by indefinite command dispatch (`AppMessage`).
//! Both are wrapped in one local enum so the driver has a single `APP` type
//! to be generic over.
//!
//! **This is a wire contract, and this file is duplicated byte-for-byte in
//! both repos** -- `Manager/src/system/tunnel_wire.rs` and
//! `portal/src/system/tunnel_wire.rs`. Payloads are bincode of Rust `serde`
//! types, so field order and types must stay identical on both sides or the
//! decode fails: not with a friendly version error, but as a freshly-connected
//! tunnel that dies during bootstrap. **Change both files in the same
//! commit.** This is the same reasoning that put `NodeKeyAnnounce` in both
//! repos in the previous migration (retired now: with the portal never
//! dialing a node, there is no key left to announce).

use artisan_middleware::aggregator::AppMessage;
use artisan_middleware::portal::PortalMessage;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TunnelMessage {
    /// One-time bootstrap traffic -- Discover, the Id exchange, then
    /// Register* -- run once at the start of every fresh tunnel connection,
    /// before any `App` traffic is sent. See `system::portal::run_bootstrap`.
    Bootstrap(PortalMessage),
    /// Command dispatch and its response. Correlated by `request_id` since,
    /// unlike `send_receive::send_message`, a `ConnectionDriver` connection
    /// has no built-in request/response pairing -- either side can push at
    /// any time, so a reply must name which request it answers.
    App(Correlated<AppMessage>),
}

/// Pairs a payload with a caller-assigned id so a reply can be matched back
/// to the request that caused it, on a connection where sends and receives
/// are otherwise unordered relative to each other.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Correlated<T> {
    pub request_id: u64,
    pub body: T,
}
