# Portal ↔ Manager Upstream API (ais_manager)

This document describes the API surface the **Portal** (or a Portal-like CLI) uses to interact with `ais_manager`.

The manager is intentionally thin: **watchdog owns lifecycle, validation, and metrics collection**. The manager mostly:

- Proxies lifecycle commands to watchdog.
- Caches “app inventory + status” snapshots for Portal queries.
- Periodically refreshes per-app stdout/stderr by reading app state files.

Source references in this repo:

- Manager TCP server: `src/network.rs`
- State lock + watchdog sync: `src/applications/watchdog_sync.rs`
- Manager info payload: `src/system/manager.rs`
- Manager → Portal registration client: `src/system/portal.rs`

---

## 1) Components and Ports

### A) Portal → Manager (control/query channel)

- **Transport:** TCP
- **Listen addr:** `0.0.0.0:9800`
- **Role:** Portal connects as a client and sends commands/queries.
- **Purpose:** Start/stop/restart apps (proxied to watchdog), request status snapshots, request manager info.

Implemented in `src/network.rs`.

### B) Manager → Portal (registration/identity channel)

- **Transport:** TCP
- **Portal endpoint (default):** `portal.arhst.net:9801` (resolved at runtime)
- **Role:** Manager connects as a client; Portal acts as a server.
- **Purpose:** identity exchange + registration heartbeat.

Implemented in `src/system/portal.rs`.

---

## 2) On-the-wire protocol: `simple_comms` (binary, Rust-first)

Both channels use the `simple_comms` protocol:

- A fixed-size header (`ProtocolHeader`) followed by a bincode-serialized payload.
- Message framing uses the sentinel **EOL bytes** `-EOL-` appended after each message.

Important note:

- Payloads are **bincode of Rust `serde` types**. This is not a language-agnostic wire format.
- For a CLI intended for testing/debugging, the simplest path is to write it in Rust and reuse:
  - `simple_comms`
  - `artisan_middleware` types (`AppMessage`, `PortalMessage`, etc.)

### Header layout (for reference)

The header is written in big-endian and is always `HEADER_LENGTH = 49` bytes:

- `version: u16`
- `flags: u8`
- `payload_length: u64`
- `reserved: u8`
- `status: u8`
- `origin_address: [u8; 4]`
- `encryption_key: [u8; 32]` (filled when `ENCRYPTED` flag is set, otherwise zeros)

Reference: `simple_comms` `ProtocolHeader` and `EOL` constant.

### Flags

Flags are bitflags in the header.

In practice for **Portal → Manager**:

- Requests can be sent with `Flags::NONE` (manager currently accepts this).
- Manager responses are typically sent with `Flags::ENCRYPTED | Flags::COMPRESSED`.

If you use `simple_comms::network::send_receive::send_message`, the library handles all encoding/decoding.

---

## 3) Portal → Manager API (TCP :9800)

### Payload type

Payloads are `artisan_middleware::aggregator::AppMessage`.

Only **one request** is supported:

- `AppMessage::Command(Command)`

Any other `AppMessage` variant is treated as illegal in this context and results in an empty error response.

### Command request

`Command` fields (Rust type: `artisan_middleware::aggregator::Command`):

- `app_id`: **application name string** (despite the name “id”)
  - Examples: `ais_manager`, `ais_gitmon`, `ais_<client_id>`
- `command_type`: one of:
  - `Start`
  - `Stop`
  - `Restart`
  - `Status`
  - `AllStatus`
  - `Info`
  - `Custom(String)` (currently returns “Request not implemented”)
- `timestamp`: `u64` (portal sets this; manager doesn’t validate it)

### Responses

The response payload is one of:

- `AppMessage::Response(CommandResponse)` for most commands
- `AppMessage::ManagerInfo(ManagerData)` for `Info`

`CommandResponse` fields:

- `app_id`: echoes request `app_id`
- `command_type`: echoes the command type
- `success`: boolean
- `message`: optional string

### Command semantics

#### `Start(app_id)`

- Proxied to watchdog `ExecuteCommand.start`.
- Response `success` mirrors watchdog `accepted`.
- On watchdog failure: `success=false`, message starts with `Watchdog unavailable:`.

#### `Stop(app_id)`

- If `app_id == "ais_manager"`: manager triggers its internal reload path and returns a “restart-style” response.
- Otherwise proxied to watchdog `ExecuteCommand.stop`.

#### `Restart(app_id)`

- If `app_id == "ais_manager"`: manager triggers its internal reload path and returns success immediately.
- Otherwise proxied to watchdog `ExecuteCommand.reload`.

#### `Status(app_id)`

- Returns a snapshot from the manager’s in-memory status cache.
- On success, `message` contains a JSON string of `AppStatus` (see “Status payloads” below).
- On miss, `success=false` and `message` explains the app wasn’t found in the store.

#### `AllStatus`

- Returns a JSON array (string) of `AppStatus` JSON objects.
- Same schema as `Status`, but for all known apps.

#### `Info`

- Returns `AppMessage::ManagerInfo(ManagerData)`.
- `ManagerData.warning` is **reserved for watchdog security trips only**:
  - The value becomes `1` if watchdog ever reports `security measures tripped` during this manager process lifetime.
  - Otherwise it is `0`.

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

The manager periodically refreshes logs by reading each app’s state file and copying:

- `app_data.state.stdout: Vec<(u64, String)>`
- `app_data.state.stderr: Vec<(u64, String)>`

State file locations probed per app:

- `/tmp/.<app>.state`
- `/opt/artisan/tmp/.<app>.state`

This is intended to capture **direct app logs** (client app stdout/stderr), separate from watchdog’s own logs API.

---

## 5) Manager → Portal registration protocol (TCP :9801)

Payloads are `artisan_middleware::portal::PortalMessage`.

Portal server should handle this sequence:

1) Manager connects and sends:
   - `PortalMessage::Discover` (typically unencrypted)
2) Portal replies:
   - `PortalMessage::IdRequest`
3) Manager replies:
   - `PortalMessage::IdResponse(Option<Identifier>)` (encrypted+compressed)
4) Portal replies:
   - `PortalMessage::IdResponse(Option<Identifier>)`
     - `Some(identifier)` to provision/update the manager identity
     - or `None` to accept the manager’s existing identity
5) Manager then reconnects and sends:
   - `PortalMessage::RegisterRequest(ManagerData)` (encrypted+compressed)
6) Portal replies:
   - `PortalMessage::RegisterResponse(bool)` on success
   - `PortalMessage::Error(String)` on failure

The manager repeats registration attempts periodically until it considers itself “linked”.

---

## 6) Practical CLI guidance (Rust)

If you want a small CLI to talk to the manager like Portal does, reuse the same crates:

- Use `simple_comms::network::send_receive::send_message` over TCP to `:9800`.
- Send `AppMessage::Command(Command { app_id, command_type, timestamp })`.
- Decode `AppMessage::Response` / `AppMessage::ManagerInfo`.

This avoids having to reimplement:

- The `simple_comms` framing (`-EOL-`), header parsing, and flag transforms.
- Bincode layouts of `AppMessage`/`PortalMessage`.

