//! Every TCP port the manager touches, in one place.
//!
//! The manager has exactly two TCP endpoints, and they are deliberately
//! nothing alike -- one is the fleet's lifeline, the other is a developer
//! convenience that should never leave the box:
//!
//! | Port | Direction | Bind / target | What it carries |
//! |------|-----------|---------------|-----------------|
//! | 9800 | **outbound** | `portal.arhst.net:9800` | the fleet tunnel -- registration once, then all portal command traffic, for the life of the process (`system::portal`) |
//! | 9825 | **inbound**  | `127.0.0.1` only | the local debug listener for `ais_manager_debug` (`crate::network`) |
//!
//! The two numbers are intentionally far apart. They used to be `:9800`
//! inbound and `:9801` outbound -- adjacent, easy to transpose, and with the
//! *public* number on the listener that must never be public. The fleet port
//! is now the memorable one (`9800`, matching the portal's
//! `system::ports::FLEET_TUNNEL_PORT`) and the debug listener sits off on
//! its own at `9825`, so a stray firewall rule or a mistyped `--addr` fails
//! loudly instead of quietly pointing at the wrong channel.
//!
//! Both are constants rather than configuration: [`PORTAL_TUNNEL_PORT`] is
//! half of a wire contract with the portal and cannot be changed on one side
//! alone; changing it means redeploying the portal *and* every manager.

/// The portal's **public fleet port**, which this manager dials out to.
///
/// This is the only port the fleet protocol uses: the manager opens one
/// persistent, `Noise_NK`-secured, full-duplex tunnel to it and keeps it open
/// (see `system::portal::maintain_tunnel`). The portal never dials back, so
/// nothing needs to be reachable *inbound* on a node for the fleet to work --
/// which is exactly why the design collapsed onto a single dialled-out
/// connection.
///
/// Mirrored in the portal repo as `portal/src/system/ports.rs`'s
/// `FLEET_TUNNEL_PORT`; the two must agree.
pub const PORTAL_TUNNEL_PORT: u16 = 9800;

/// Where the local debug listener binds: **loopback only**, on purpose.
///
/// `ais_manager_debug` dials this for manual diagnostics and overrides on the
/// same host (`crate::network::process_tcp`). It shares the manager's command
/// execution path with the fleet tunnel but is no part of the fleet protocol
/// -- nothing off-host should ever reach it, and binding `127.0.0.1` rather
/// than `0.0.0.0` is what enforces that regardless of firewall state.
pub const DEBUG_LISTENER_ADDR: &str = "127.0.0.1:9825";
