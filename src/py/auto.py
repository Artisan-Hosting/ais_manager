#!/usr/bin/env python3
import subprocess
import sys
import socket

def get_active_network_pids():
    """
    Use 'ss' to list active network connections and extract unique PIDs.
    Returns a set of PIDs.
    """
    try:
        # -p: show process info, -u: UDP, -t: TCP, -n: numeric, -a: all sockets
        result = subprocess.run(
            ["ss", "-ptuna"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=True
        )
    except subprocess.CalledProcessError as e:
        print("Failed to run ss command:", e.stderr)
        sys.exit(1)

    pids = set()

    for line in result.stdout.splitlines():
        if "pid=" in line:
            parts = line.split()
            for part in parts:
                if "pid=" in part:
                    pid_str = part.split("pid=")[-1].split(",")[0]
                    if pid_str.isdigit():
                        pids.add(int(pid_str))

    return pids

def format_bytes_as_hex(byte_data):
    """Format bytes as space-separated lowercase hex."""
    return ' '.join(f'{byte:02x}' for byte in byte_data)

def insert_pid_into_bpf_map(pid, pinned_map_path):
    """Insert the PID into the pinned BPF map using bpftool."""
    key_bytes = pid.to_bytes(4, byteorder="little")
    key_hex = format_bytes_as_hex(key_bytes)

    value_bytes = bytes(16)  # Zeroed initial value (TrafficStats struct)
    value_hex = format_bytes_as_hex(value_bytes)

    cmd = [
        "/sbin/bpftool", "map", "update", "pinned", pinned_map_path,
        "key", "hex", key_hex,
        "value", "hex", value_hex,
    ]

    try:
        subprocess.run(cmd, check=True, text=True)
        print(f"✅ Inserted PID {pid} into BPF map.")
    except subprocess.CalledProcessError as e:
        print(f"❌ Failed to insert PID {pid}: {e}")

def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <pinned_map_path>")
        print(f"Example: {sys.argv[0]} /sys/fs/bpf/pid_traffic_map")
        sys.exit(1)

    pinned_map_path = sys.argv[1]

    print("🔍 Scanning for active network PIDs...")
    pids = get_active_network_pids()

    if not pids:
        print("⚠️  No active network PIDs found.")
        sys.exit(0)

    print(f"Found {len(pids)} active network PIDs: {pids}")

    for pid in pids:
        insert_pid_into_bpf_map(pid, pinned_map_path)

    print("🎉 All active network PIDs inserted.")

if __name__ == "__main__":
    main()