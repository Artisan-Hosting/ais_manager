# Portal ↔ Manager Upstream API (ais_manager)

This document describes the API surface the **Portal** (or a Portal-like CLI)
uses to interact with `ais_manager`, and how the manager gets that connection
up and keeps it up.

The manager is intentionally thin: **watchdog owns lifecycle, validation, and
metrics collection**. The manager mostly:

- Proxies lifecycle commands to watchdog.
- Caches "app inventory + status" snapshots for Portal queries.
- Periodically refreshes per-app stdout/stderr by reading app state files.

The portal's half of this story — the accept loop, the live tunnel registry,
and how the rest of the portal reaches a node — is documented in the portal
repo at `portal/docs/fleet_tunnel.md`. The two are meant to be read together.

Source references in this repo:

| Concern | File |
|---|---|
| The fleet tunnel: supervision, bootstrap, dispatch | `src/system/portal.rs` |
| Command execution, shared by both channels | `src/network.rs` |
| The wire envelope multiplexed over the tunnel | `src/system/tunnel_wire.rs` |
| `Noise_NK` key material for both channels | `src/system/noise.rs` |
| Every TCP port this manager dials or binds | `src/system/ports.rs` |
| State lock + watchdog sync | `src/applications/watchdog_sync.rs` |
| Manager info payload | `src/system/manager.rs` |
| Local debug CLI | `src/bin/ais_manager_debug.rs` (see `notes.md`) |

---

## 1) Components and Ports

### Ports at a glance

| Port | Direction | Endpoint | What it is |
|---|---|---|---|
| **9800** | **outbound** | `portal.arhst.net:9800` | The fleet tunnel. Everything the portal and this manager say to each other. |
| **9825** | inbound | `127.0.0.1` **only** | The local debug listener. Not part of the fleet protocol. |

Nothing needs to be reachable *inbound* on a node for the fleet to work. That
is the single most important operational consequence of this design: the
manager dials out, the portal never dials back, and a node behind NAT or a
closed firewall is fully manageable.

> **Renumbered.** These used to be `:9801` outbound and `:9800` inbound —
> adjacent numbers, with the public-looking one on the listener that must never
> be public. The fleet port is now `9800` on both sides, and the debug listener
> moved out to `9825`. **Portal and manager must be deployed together across
> this change**; a manager still dialing `:9801` will find nothing listening.

### A) The fleet tunnel (Manager → Portal)

- **Transport:** TCP, one persistent connection per manager, full-duplex via
  `simple_comms::network::driver::ConnectionDriver`.
- **Portal endpoint:** `portal.arhst.net:9800`, resolved at runtime
  (`ports::PORTAL_TUNNEL_PORT`).
- **Role:** Manager connects out; Portal accepts and holds the connection open
  for as long as the manager stays reachable.
- **Noise_NK role:** Manager is the **initiator**, portal the **responder**.
- **Purpose:** everything — identity exchange and registration once at
  connection start (§5), then indefinite command dispatch in both directions
  (§3) for the rest of that connection's life.

Implemented in `src/system/portal.rs`; §7 covers how it is supervised.

There used to be a second, separate TCP channel for commands, with the portal
dialing out to each node. That is gone; see §2.1 for why it is gone rather than
merely relocated.

### B) Local debug listener (loopback only)

- **Transport:** TCP, request/response (`simple_comms::network::send_receive::send_message`).
- **Listen addr:** `127.0.0.1:9825` — **not** reachable off the host, and bound
  to loopback rather than firewalled off it.
- **Role:** a debugging/manual-override tool (`ais_manager_debug`) connects as
  a client on the same machine.
- **Noise_NK role:** the tool is the **initiator**, manager the **responder**.
- **Purpose:** the same command execution as the fleet tunnel
  (`network::command_processor`, shared verbatim by both), for local
  troubleshooting without going through the portal at all.

One connection here carries exactly one command — handshake, request, reply,
close. There is no correlation id and no long-lived state, because there is no
full-duplex driver involved.

Implemented in `src/network.rs`; see §6 for using it.

---

## 2) On-the-wire protocol: `simple_comms` 2.x (binary, Rust-first)

Both the fleet tunnel and the local debug listener use the `simple_comms`
protocol:

- A fixed-size header (`ProtocolHeader`) followed by a bincode-serialized payload.
- Every connection opens with a **`Noise_NK` handshake** before any application
  message (see §2.1). There is no unauthenticated path onto either channel.
- Framing ends each message with the sentinel `-EOL-`, owned by
  `ProtocolMessage::write_to` / `read_from` — callers never append it or call
  `format()` / `from_bytes` by hand.
- The fleet tunnel additionally runs
  `simple_comms::network::driver::ConnectionDriver` on top of the handshake,
  turning one connection full-duplex for its whole life (heartbeats and key
  rotation handled internally) instead of the strict one-send-one-reply
  `send_receive::send_message` the debug listener still uses.

Important note:

- Payloads are **bincode of Rust `serde` types**. This is not a language-agnostic wire format.
- For a CLI intended for testing/debugging, the simplest path is to write it in Rust and reuse:
  - `simple_comms`
  - `artisan_middleware` types (`AppMessage`, `PortalMessage`, etc.)
  - `ais_manager_debug` already does exactly this against the local debug
    listener; see `notes.md`. Talking to the fleet tunnel the way the portal
    does needs the `ConnectionDriver` side of the library, not `send_message`.

### 2.1) Handshake and key distribution

Every connection is `Noise_NK`: the **responder** holds a long-term static
keypair and the **initiator** must already know that responder's public key.

The portal never dials a node — every manager dials it — so the portal plays
exactly one Noise role on the fleet path: **responder**, with its keypair at
`/opt/artisan/portal_identity.key`. Its public half is pinned out-of-band on
every manager as `/opt/artisan/portal.pub`. The manager is a Noise
**initiator only** there, which means it needs no static identity of its own
for this connection at all — an initiator in `Noise_NK` never does.

An earlier version of this protocol had the portal dialing each manager on a
separate channel, which meant the portal had to learn each manager's public key
at runtime — a whole `NodeKeyAnnounce` subsystem. That problem does not have a
nicer answer now; it does not exist, because the portal never dials a node.

The manager's own keypair (`/opt/artisan/manager_identity.key`) still exists,
but scoped down to authenticating the *local debug listener* (§1B) — there the
manager is the responder, to the debug CLI's initiator. Nothing on the fleet
side depends on it, so regenerating it costs nothing beyond re-reading the key
for the local CLI.

Regenerating the **portal's** identity file is a different matter entirely: it
is a breaking act that locks out every manager in the fleet at once, until the
new public key is re-pinned on each of them.

`AIS_PORTAL_PUBKEY` overrides `/opt/artisan/portal.pub` with a hex key directly
for tests and one-off debugging against a non-production portal. The manager
logs a warning when it takes that path; the file is the supported production
mechanism.

### Header layout (for reference)

The header is written in big-endian and is always `HEADER_LENGTH = 50` bytes:

- `version: u16`
- `flags: u8` (a `ConnectionParams` bitfield)
- `payload_length: u64`
- `msg_type: u8` (`Hello`, `HelloAck`, `Open`, `OpenAck`, `Data`, `Heartbeat`,
  `Close`, `Error`, `Rekey` — application traffic is `Data`)
- `reserved: u8`
- `status: u8`
- `origin_address: [u8; 4]`
- `encryption_key: [u8; 32]` (a `RecordMeta` overlay on an encrypted connection,
  otherwise a fallback single-message key)

Reference: `simple_comms` `ProtocolHeader`, and `docs/PROTOCOL.md` in that crate.

### Connection params

What used to be per-message `Flags` is now a **connection-wide**
`ConnectionParams` baseline. The initiator declares it on `Hello`; the responder
adopts it; `send_message` then uses `conn.params` automatically rather than
taking flags per call.

Both the fleet tunnel and the debug listener are established with
`ConnectionParams::OPTIMIZED` (`COMPRESSED | ENCRYPTED | ENCODED | SIGNATURE`),
deliberately **without** `INSECURE` — so neither peer can be talked down to
weaker parameters mid-connection via a `SIDEGRADE` response.
`ais_manager_debug --insecure` adds that bit for debugging, which also relaxes
the protocol-version in-band check.

---

## 3) Command dispatch (the fleet tunnel's steady state, and the debug listener)

### Payload type

The underlying application payload is `artisan_middleware::aggregator::AppMessage`
either way. On the fleet tunnel it travels wrapped as
`TunnelMessage::App(Correlated<AppMessage>)` (see `system::tunnel_wire`) —
`request_id` lets a reply be matched back to the request that caused it on a
connection where either side can push at any time; the debug listener, being
plain request/response, carries bare `AppMessage`.

The portal assigns `request_id`s; the manager only ever **echoes back the id it
was given**. Ids are monotonic per connection and reset on reconnect, which is
safe because the portal's pending-request map dies with the connection too.

Only **one request** is supported:

- `AppMessage::Command(Command)`

Any other `AppMessage` variant is treated as illegal in this context and results
in an error response. On the tunnel this is answered with a
`CommandResponse { success: false, command_type: Custom("illegal message") }`
rather than dropping the connection, so a confused portal gets a reply instead
of a hang.

### Command request

`Command` fields (Rust type: `artisan_middleware::aggregator::Command`):

- `app_id`: **application name string** (despite the name "id")
  - Examples: `ais_manager`, `ais_gitmon`, `ais_<client_id>`
- `command_type`: one of:
  - `Start`
  - `Stop`
  - `Restart`
  - `Status`
  - `AllStatus`
  - `Info`
  - `Custom(String)` (currently returns "Request not implemented")
- `timestamp`: `u64` (portal sets this; manager doesn't validate it)

### Responses

The response payload is one of:

- `AppMessage::Response(CommandResponse)` for most commands
- `AppMessage::ManagerInfo(ManagerData)` for `Info`

`CommandResponse` fields:

- `app_id`: echoes request `app_id`
- `command_type`: echoes the command type
- `success`: boolean
- `message`: optional string

Errors are reported **in band** wherever possible — a `CommandResponse` with
`success: false` — rather than by failing the connection, so a failed command
never looks like a failed node.

### Command semantics

All of these are implemented once, in `network::command_processor`, and shared
verbatim by the fleet tunnel and the debug listener. Neither channel interprets
commands itself, which is what makes `ais_manager_debug` a faithful stand-in
for the portal when diagnosing behavior: the only difference is transport.

Before any command runs, the manager waits up to 1 s on its network control
lock. If that times out — the manager is mid-reload — the command is answered
with `success: false` and "Server not accepting requests" rather than queued.

#### `Start(app_id)`

- Proxied to watchdog `ExecuteCommand.start`.
- Response `success` mirrors watchdog `accepted`.
- On watchdog failure: `success=false`, message starts with `Watchdog unavailable:`.

#### `Stop(app_id)`

- If `app_id == "ais_manager"`: manager triggers its internal reload path and returns a "restart-style" response.
- Otherwise proxied to watchdog `ExecuteCommand.stop`.

#### `Restart(app_id)`

- If `app_id == "ais_manager"`: manager triggers its internal reload path and returns success immediately.
- Otherwise proxied to watchdog `ExecuteCommand.reload`.

A manager asked to stop or restart *itself* reloads instead, because a process
cannot usefully stop the thing answering the request. The reload drops the
tunnel, which the portal sees as a disconnect and the manager repairs by
redialing (§7).

#### `Status(app_id)`

- Returns a snapshot from the manager's in-memory status cache (refreshed from
  watchdog by `applications::watchdog_sync`), so it never blocks on watchdog.
- On success, `message` contains a JSON string of `AppStatus` (see "Status payloads" below).
- On miss, `success=false` and `message` explains the app wasn't found in the store.

#### `AllStatus`

- Returns a JSON array (string) of `AppStatus` JSON objects.
- Same schema as `Status`, but for all known apps.
- This is the portal's high-frequency poll (roughly once a second), so it is
  deliberately a cache read with a 2 s lock timeout and nothing more.

#### `Info`

- Returns `AppMessage::ManagerInfo(ManagerData)`: version, hostname, local
  address, git config, system/client app counts, identity, and uptime.
- `ManagerData.warning` is **reserved for watchdog security trips only**:
  - The value becomes `1` if watchdog ever reports `security measures tripped` during this manager process lifetime.
  - Otherwise it is `0`.
  - It is sticky for the life of the process — once tripped, it stays `1` until
    the manager restarts.
- Missing git credentials are downgraded to an empty `git_config` with a
  warning rather than failing the command; watchdog validates runnable apps, and
  a credentials problem should not make a node look unreachable.

---

## 4) Status payloads (for `Status` / `AllStatus`)

The `message` field contains JSON serialized from:

- `artisan_middleware::aggregator::AppStatus`

Important fields in `AppStatus`:

- `app_id`: string (stable-ish ID derived from machine identity + app name)
- `git_id`: string (empty for system apps; client apps use `ais_` prefix stripping)
- `expected_status`: enum `Status` (manager sets defaults; watchdog updates actual status)
- `timestamp`: `u64` (watchdog `last_updated` is used)
- `metrics`: optional (populated from watchdog when app is in an active state)
- `app_data`: `ApplicationConfig` which includes:
  - `state`: `AppState` (contains `status`, `pid`, `last_updated`, plus `stdout`/`stderr`)
  - `config`: `AppConfig`

### Logs

The manager periodically refreshes logs by reading each app's state file and copying:

- `app_data.state.stdout: Vec<(u64, String)>`
- `app_data.state.stderr: Vec<(u64, String)>`

State file locations probed per app:

- `/tmp/.<app>.state`
- `/opt/artisan/tmp/.<app>.state`

This is intended to capture **direct app logs** (client app stdout/stderr), separate from watchdog's own logs API.

---

## 5) Bootstrap sequence (start of every fleet tunnel connection, `:9800`)

Payloads are `artisan_middleware::portal::PortalMessage`, wrapped as
`TunnelMessage::Bootstrap(PortalMessage)`. This is the **first thing** that
happens on a freshly-established tunnel connection, before any `App` (command)
traffic — see `Manager::system::portal::run_bootstrap` /
`portal::system::tunnel::run_bootstrap_responder` for the implementations this
describes.

1) Manager sends:
   - `PortalMessage::Discover`
2) Portal replies:
   - `PortalMessage::IdRequest`
3) Manager replies:
   - `PortalMessage::IdResponse(Option<Identifier>)`
4) Portal replies:
   - `PortalMessage::IdResponse(Option<Identifier>)`
     - `Some(identifier)` to provision a new manager identity
     - or `None` to accept the manager's existing identity
5) Manager sends, **on the same connection**:
   - `PortalMessage::RegisterRequest(ManagerData)`
6) Portal replies:
   - `PortalMessage::RegisterResponse(bool)` on success
   - `PortalMessage::Error(String)` on failure

Anything other than a `Bootstrap` message during this phase — command traffic, a
stray `Open`/`OpenAck`, or the connection closing — is itself an error and ends
the connection. The manager verifies any identity it adopts (`Identifier::verify`)
before writing it to disk, and the portal verifies it again at step 5; either
side rejecting means the connection ends with a `PortalMessage::Error` on the
wire, so the reason is visible in both logs rather than looking like a network
fault.

Once `RegisterResponse(true)` arrives, the connection moves on to indefinite
`App` traffic (§3) for the rest of its life. There is no `NodeKeyAnnounce` step
— see §2.1 for why that is gone rather than moved — and no separate
acknowledgement after a freshly-issued identity, which existed only to satisfy
`send_message`'s blocking request/response contract on the portal's old
responder path.

The manager re-runs this sequence, unconditionally, at the start of **every**
fresh physical connection — not just the first one ever. That's deliberate:
under the old two-channel design a manager that had linked once wouldn't
re-register, so a portal that lost its state (e.g. a restart) couldn't reach
that node until its next scheduled attempt. A fresh tunnel connection is now
the only way the portal learns a node is reachable at all, so re-registering
every time closes that gap instead of leaving it as a timing window.
Re-registration is cheap: `RegisterRequest` is upsert-safe on the portal side.

---

## 6) Practical CLI guidance (Rust)

If you want a small CLI to talk to **a manager directly on the same host** the
way `ais_manager_debug` does, reuse the same crates against the local debug
listener:

- Use `simple_comms::network::send_receive::send_message` over TCP to
  `127.0.0.1:9825`.
- Complete a `Noise_NK` handshake as the **initiator**, pinning the manager's
  public key — the `public=` line of `/opt/artisan/manager_identity.key` on that
  host. A wrong key fails at the handshake and looks like a connection error.
- Send `AppMessage::Command(Command { app_id, command_type, timestamp })`.
- Decode `AppMessage::Response` / `AppMessage::ManagerInfo`.

This avoids having to reimplement:

- The `simple_comms` framing (`-EOL-`), header parsing, and flag transforms.
- Bincode layouts of `AppMessage`/`PortalMessage`.

Talking to a manager **the way the portal does** (i.e. commanding an arbitrary
node in the fleet, from off-host) means being the portal: opening the fleet
tunnel's `ConnectionDriver` side, running the bootstrap sequence (§5), and
issuing `Correlated<AppMessage>` requests, not `send_message`. There's no
supported shortcut for that today outside the portal binary itself — and there
is no inbound port on a node to aim such a tool at in any case.

### Repo-provided debug CLI

This repo includes a feature-gated CLI binary:

- Build: `cargo build --features debug-cli --bin ais_manager_debug`
- Run: `cargo run --features debug-cli --bin ais_manager_debug -- --help`

---

## 7) Tunnel supervision: connecting, reconnecting, and giving up

`system::portal::maintain_tunnel` is spawned once from `main.rs` and runs for
the life of the process. Its whole job is that **exactly one tunnel is up
whenever one can be**.

Each pass around the supervisor loop:

1. **Resolve** `portal.arhst.net` (`get_portal_addr`), recording every address
   it yields as a dial candidate paired with `:9800`. The lookup goes to a
   hard-coded **nameserver** — `10.1.0.1` in production, `192.168.122.169` in
   development — not to the node's own `/etc/resolv.conf`, so fleet names stay
   under Artisan's control on a node with broken or hijacked DNS. (Note that
   this IP is the resolver, not the portal.) Resolution happens every pass
   rather than once at startup, so a portal that moves — failover, re-IP — is
   picked up by the next redial instead of needing a manager restart.
2. **Dial each candidate in turn** until one connects and completes bootstrap.
3. **Stay** on a connection that worked. When it eventually ends, do *not* move
   on to the next candidate — that address is known-good, so fall through and
   redial it from the top.
4. **Back off** before the next pass: 5 s if a tunnel was up and died, 30 s if
   nothing connected at all. Failing DNS or a down portal therefore costs one
   attempt every 30 s rather than a spin.

`run_tunnel` handles one connection end to end and returns when it is over:

- `Ok(())` — it connected, ran, and the peer eventually went away (`Close`,
  heartbeat timeout, or fatal I/O). Normal; redial.
- `Err(_)` — it never connected, or never finished bootstrapping. Logged with
  the address, and the next candidate is tried.

Note what is *absent*: there is no queue of commands waiting for a tunnel, and
no replay after reconnect. A command the portal issued into a dying tunnel is
failed there, on the portal side, and the portal reissues on its own schedule.
That keeps the manager stateless with respect to connectivity — reconnecting is
always a clean slate.

---

## 8) Failure modes and what they look like

| Symptom (manager log) | Likely cause | Where to look |
|---|---|---|
| `Failed to establish a tunnel to portal @ …` every 30 s | Portal down, `:9800` unreachable, or DNS wrong | Network path to the portal; is the portal's listener up? |
| Handshake failure on every attempt | This node pins a stale/wrong `/opt/artisan/portal.pub` | §2.1 — re-copy the portal's `public=` line |
| `The manager cannot establish a Noise_NK connection to the portal without it` at startup | `/opt/artisan/portal.pub` missing entirely | Distribute the portal public key to this node |
| `Unexpected message during bootstrap: …` | Portal/manager version skew in the bootstrap state machine | §5, and whether both sides ship the same `tunnel_wire` |
| Tunnel connects, then drops within seconds, repeatedly | Registration rejected (identity verification), or a decode mismatch | Portal log will name the reason; check `Identifier` state in `/opt/artisan` |
| `Unexpected bootstrap message after registration` | Portal-side state machine confusion | Portal `system::tunnel::run_dispatch_loop` |
| `Portal sent an illegal message over the tunnel` | Portal sent a non-`Command` `AppMessage` | Portal caller that built the message |
| Tunnel stays up but every command answers "Server not accepting requests" | Manager is paused mid-reload and the control lock never resumed | `system::control::Controls` |
| Commands answer `Watchdog unavailable: …` | Watchdog is down or its socket is gone | `/tmp/artisan_watchdog.sock`, watchdog service state |
| Node shows in the portal but all statuses are stale/empty | Watchdog sync loop failing, not a comms problem | `applications::watchdog_sync` |

A quick triage rule: if `ais_manager_debug` on the box answers correctly but the
portal shows the node as down, the problem is the tunnel (network, keys,
registration). If the debug CLI is *also* wrong, the problem is below the comms
layer — watchdog, or the status cache.
