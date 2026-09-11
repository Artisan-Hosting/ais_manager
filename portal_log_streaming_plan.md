# Implementation Plan: Log Streaming on the Portal Application

This document outlines the changes required on the **Portal** side to properly support and consume the custom chunk-based log streaming protocol implemented in the Manager.

---

## Architecture Overview

1. **Manager Constraint**: The Manager now limits `AllStatus` and `Status` response payloads to the **latest 20 lines** of logs to keep standard query payloads safely below the Noise protocol's **64 KB** packet limit.
2. **Streaming Protocol**: When full logs are needed, the Portal initiates a `StreamLogs` command. The Manager then streams the complete logs back over the existing duplex fleet tunnel using **100-line encrypted chunks** (`LogStreamChunk`) and terminates with a `LogStreamEnd` packet.

---

## Step-by-Step Portal Changes

### Step 1: Update the Shared Crate Dependency (if applicable)
Since we added `serde_json = "1.0"` to the Manager dependencies to serialize the chunk vectors, ensure the Portal also includes `serde_json` in its `Cargo.toml`.
*(Note: `src/system/tunnel_wire.rs` was untouched, so the binary bincode/serde contract remains fully compatible.)*

---

### Step 2: Define Log Structures in the Portal
Define the matching `LogLine` structure on the Portal to deserialize the JSON payloads contained in the `LogStreamChunk` messages:

```rust
#[derive(serde::Deserialize, Debug, Clone)]
pub struct LogLine {
    pub stream: String, // "stdout" or "stderr"
    pub timestamp: u64,
    pub line: String,
}
```

---

### Step 3: Implement Log Stream State Management
In the Portal's fleet tunnel driver module (usually near where connection sessions and the pending-request map are maintained):

1. **Active Stream Registry**: Implement a registry or cache (e.g., `DashMap<u64, tokio::sync::mpsc::UnboundedSender<LogLine>>`) to hold active log stream channels indexed by the connection's `request_id`.
2. **Initiating the Stream**:
   When the user requests full logs for an application:
   - Generate a unique, monotonic `request_id`.
   - Create an unbounded channel `(tx, rx)`.
   - Insert `tx` into the **Active Stream Registry** under `request_id`.
   - Send the initial command over the manager's tunnel:
     ```rust
     AppMessage::Command(Command {
         app_id,
         command_type: CommandType::Custom("StreamLogs".to_string()),
         timestamp,
     })
     ```
   - Keep the API request handler waiting on the `rx` stream.

---

### Step 4: Handle Multi-Message Responses in Dispatch Loop
Update the Portal's central tunnel dispatch listener loop (which processes incoming replies matching a pending `request_id`) to intercept the custom streaming responses:

```rust
// Inside the Portal's dispatch loop where AppMessage::Response(response) is matched:
match response.command_type {
    CommandType::Custom(ref cmd) if cmd == "LogStreamChunk" => {
        if let Some(message_str) = response.message {
            if let Ok(chunks) = serde_json::from_str::<Vec<LogLine>>(&message_str) {
                if let Some(tx) = active_streams.get(&request_id) {
                    for log_line in chunks {
                        let _ = tx.send(log_line);
                    }
                }
            }
        }
        // DO NOT remove from pending-requests map yet; more chunks are coming!
    }
    CommandType::Custom(ref cmd) if cmd == "LogStreamEnd" => {
        // Clean up the stream
        active_streams.remove(&request_id);
        pending_requests.remove(&request_id);
    }
    CommandType::Custom(ref cmd) if cmd == "LogStreamError" => {
        log::error!("Manager failed to stream logs: {:?}", response.message);
        active_streams.remove(&request_id);
        pending_requests.remove(&request_id);
    }
    _ => {
        // Handle standard one-off responses (Start, Stop, AllStatus, etc.)
        resolve_standard_request(request_id, response);
    }
}
```

---

### Step 5: Expose Logs Stream via Portal API (HTTP/SSE)
For the upstream Portal web interface or CLI:

* **Option A: Aggregated (Simple)**
  The Portal waits until `LogStreamEnd` is received, joins all received log lines chronologically, formats them, and returns them as a single JSON response to the user.
  
* **Option B: Real-Time Stream (Modern)**
  Expose an HTTP Server-Sent Events (SSE) endpoint (e.g., `GET /api/apps/:id/logs/stream`). Read from the `rx` channel of the Active Stream Registry and immediately flush individual log lines to the operator's browser in real-time as they arrive!
