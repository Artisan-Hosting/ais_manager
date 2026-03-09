ais_manager_debug (feature: debug-cli)

USAGE:
  ais_manager_debug [--addr HOST:PORT] [--insecure] <command> [args...]

COMMANDS:
  start <app>       Start an app (proxied to watchdog)
  stop <app>        Stop an app (proxied to watchdog; ais_manager triggers reload)
  restart <app>     Restart/reload an app (proxied to watchdog; ais_manager triggers reload)
  status <app>      Get status snapshot (returns JSON string)
  all-status        Get all status snapshots (returns JSON array string)
  info              Get ManagerData

FLAGS:
  --addr HOST:PORT  Default: 127.0.0.1:9800
  --insecure        Disable version-in-band check (simple_comms)