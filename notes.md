# ais_manager_debug (feature: debug-cli)

A local operator CLI for the manager's **loopback-only** debug listener
(`127.0.0.1:9825`). It only works from the same host as the manager.

This is **not** a portal, and cannot be pointed at one. The portal reaches a
manager over the fleet tunnel the *manager* dials out on (`:9800`); nothing
dials in. What makes this tool useful is that the debug listener hands your
command to the very same `network::command_processor` the tunnel uses — so a
result here is what the portal would have gotten, minus the transport.

See `portal_upstream_api.md` §1B and §6 for the full picture.

## Build and run

```
cargo build --features debug-cli --bin ais_manager_debug
cargo run   --features debug-cli --bin ais_manager_debug -- --help
```

## Usage

```
ais_manager_debug [--addr HOST:PORT] [--pubkey HEX] [--insecure] <command> [args...]
```

### Commands

| Command | Effect |
|---|---|
| `start <app>` | Start an app (proxied to watchdog) |
| `stop <app>` | Stop an app (proxied to watchdog; `ais_manager` triggers reload) |
| `restart <app>` | Restart/reload an app (proxied to watchdog; `ais_manager` triggers reload) |
| `status <app>` | Status snapshot (JSON string) |
| `all-status` | All status snapshots (JSON array string) |
| `info` | `ManagerData` for this host |

`<app>` is an application *name*: `ais_manager`, `ais_gitmon`, `ais_<client_id>`.

### Flags

| Flag | Meaning |
|---|---|
| `--addr HOST:PORT` | Default `127.0.0.1:9825`. Deliberately **not** the portal's `:9800` fleet port, which this tool cannot speak. |
| `--pubkey HEX` | The target manager's `Noise_NK` public key (64 hex chars). Defaults to the `public=` line of this host's `/opt/artisan/manager_identity.key`, which is what you want when talking to the manager on this machine. |
| `--insecure` | Declare `ConnectionParams::INSECURE`: disables the version-in-band check and permits `SIDEGRADE` renegotiation. Debugging only. |

## Gotchas

- **Handshake failures are usually a key problem, not a network problem.** The
  CLI is a `Noise_NK` initiator and must pin the manager's public key. A wrong
  `--pubkey` fails at the handshake and reads like a connection error.
- **The manager generates its identity on first start.** If
  `/opt/artisan/manager_identity.key` does not exist yet, start `ais_manager`
  once or pass `--pubkey` explicitly.
- **"Server not accepting requests"** means the manager is paused mid-reload,
  not that the command was rejected.
- **`Watchdog unavailable: …`** means the failure is below the manager —
  watchdog is down or its socket is gone.
