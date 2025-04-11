#!/usr/bin/env python3
import subprocess
import sys

def update_pid_map(pid: int, pinned_map_path: str):
    """
    Updates the pinned BPF map by inserting an entry for the provided PID.

    The key is the PID (4 bytes, little endian),
    and the value is a 16-byte zeroed structure (struct traffic_stats).
    """
    # Convert the PID (integer) to a 4-byte little-endian hex string with spaces
    key_bytes = pid.to_bytes(4, byteorder="little")
    key_hex = ' '.join(f'{byte:02x}' for byte in key_bytes)

    # 16-byte zero-initialized value (traffic_stats struct)
    value_bytes = bytes(16)  # Proper zeroed bytes
    value_hex = ' '.join(f'{byte:02x}' for byte in value_bytes)

    # Build the bpftool command.
    cmd = [
        "/sbin/bpftool", "map", "update", "pinned", pinned_map_path,
        "key", "hex", key_hex,
        "value", "hex", value_hex,
    ]

    print("Running command:")
    print(" ".join(cmd))

    try:
        subprocess.run(cmd, check=True)
        print(f"✅ Successfully added PID {pid} to the BPF map at {pinned_map_path}.")
    except subprocess.CalledProcessError as e:
        print("❌ Failed to update the BPF map:")
        print(e)
        sys.exit(1)

def main():
    if len(sys.argv) != 3:
        print("Usage: {} <pid> <pinned_map_path>".format(sys.argv[0]))
        print("Example: {} 1234 /sys/fs/bpf/pid_traffic_map".format(sys.argv[0]))
        sys.exit(1)

    try:
        pid = int(sys.argv[1])
    except ValueError:
        print("Error: PID must be an integer.")
        sys.exit(1)

    pinned_map_path = sys.argv[2]

    update_pid_map(pid, pinned_map_path)

if __name__ == "__main__":
    main()