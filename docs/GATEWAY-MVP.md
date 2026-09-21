# Application gateway MVP

Goal: one USB owner supports remote firmware updates and local/HTTP application
control, with console output and application events on the same connection.

## Incremental plan

- [x] Versioned framing, bounded decoder and corruption tests.
- [x] Optional ESP-IDF component and ESP32-S3 USB Serial/JTAG example.
- [x] Host application session library, worker integration, HTTP and CLI.
- [x] Real board: flash, negotiate, requests, events and stdio concurrently.
- [x] Record measurements, limits and reproducible usage.
- [x] Host duplex scheduling: console input during a pending command, reads during writes.

Baseline for this spike: ESP-IDF **5.5.3**, ESP32-S3 native USB Serial/JTAG.
Android, automatic protocol negotiation, hot switching back to raw, multiple
in-flight application requests and a stable published SDK are later milestones.
No change to ordinary firmware's raw monitor/flash requirements.

## Ownership and lifecycle

The existing per-device worker remains the sole USB owner. Protocol/session code
is independent of HTTP and native serial types. Library calls and HTTP route to
the same worker. An explicit application connect negotiates a fresh session;
firmware initialization never waits for a host. Flash/reset invalidates the
session. Reconnect requires explicit application connect and never retries an
application command: a timeout may mean the command ran but its response was lost.

Each device admits one application request and one console input operation at
once. A slow command does not block monitor keyboard input; logs, events and
responses keep flowing during long writes. A second request or input receives
`device_busy`. Connect, monitor attachment, flash and reset remain exclusive.

The worker alternates bounded writes (at most 256 bytes) with reads. Console
payloads use 256-byte frames; senders switch only between complete frames. The
transport must use short bounded read/write timeouts. No blocking drain/flush is
used: input success means bytes were accepted by the driver, not acknowledged by
the application. Write failures/timeouts fail pending work without replaying it;
a response timeout does not cancel unrelated console input. Running operations
are retained while completed-operation history is evicted.

The firmware has
one TX owner, bounded high-priority control and lower-priority console queues.
Priority applies between frames, not to bytes already buffered by USB. Overload
may drop console chunks; counters expose the loss. Control is not hard real time.

## Experimental wire protocol v1

HDLC-style delimiters: 0x7e at both ends; 0x7e and 0x7d inside a frame become
0x7d followed by byte XOR 0x20. Unescaped body:

| Field | Encoding |
| --- | --- |
| version | u8, 1 |
| kind | u8: hello=1, hello_ack=2, console=3, request=4, response=5, event=6, error=7 |
| session | u64 little endian, nonzero nonce chosen by host |
| request | u32 little endian, zero except hello/ack and request/response/error |
| payload | 0..1024 bytes |
| checksum | CRC-32/ISO-HDLC of preceding body, u32 little endian |

Hello/ack use request=0. A hello establishes a new session, including when the
previous host disappeared. Commands require the current session and nonzero
request IDs. No replay across sessions. Ack contains JSON application identity,
protocol version, boot identity and supported methods. MVP request payload is
JSON {"method":"...","params":...}; response/error/event payloads are JSON.
Console payload is arbitrary bytes (including ANSI and partial lines).

Firmware stays in raw console mode until explicit hello. Connecting is an
opt-in operation for compatible firmware, not a probe sent to arbitrary REPLs.
Reset restores raw mode. ROM/panic/direct driver writes are outside stdio capture;
unframed bytes terminate a negotiated host session and are shown as raw output.
Attach the console once at startup, before application tasks/REPLs; direct USB
writers and independently redirected FILE streams are unsupported.

## Host interface

`POST /v1/application` accepts an `Idempotency-Key` and this JSON:

```json
{
  "device_id": "DEVICE_ID",
  "monitor_baud": 115200,
  "timeout_ms": 2000,
  "command": {"method": "echo", "params": {"hello": "device"}}
}
```

Omit `command` (or set it to null) to connect explicitly. The response is HTTP
202 with an ordinary Operation. Poll `/v1/operations/{id}` until completion;
`result` is handshake identity or the command's JSON response. Application
errors/timeouts produce failed operations; malformed requests fail admission.
Same-key retries reuse the retained operation rather than sending another
command. Existing bounded retention still applies. Busy devices return 409.

The existing `/v1/events` and `/v1/stream` expose `application_connected`,
`application_disconnected`, `application_event`, `application_protocol_error`,
and the existing decoded `raw`/`log` events. A command response is in its own
operation, not consumed from a shared subscription. Monitor stdin uses console
frames in an application session. Reset/flash invalidates the session before
opening the bootloader. USB reopen restores monitor transport, not an application
session: call connect again. Other clients must observe these lifecycle events.

`Service::submit_application` is the embedded counterpart; `get_operation` and
`application_events` expose results and cursors without an HTTP listener.
These scheduling guarantees apply to the service through both lib and HTTP.
The low-level `application::Session` convenience methods remain synchronous;
embedded applications needing concurrent operations should use `Service`.
Protocol/session modules are independent of HTTP. The service still resides in
`server.rs`; moving its generic scheduler into a separate core crate and a
published remote client SDK are follow-up refactors, not MVP promises.

## JSON profile

The MVP firmware uses cJSON. Strings/keys cannot contain embedded NUL; use base64
for binary data. Numeric magnitude is limited to 2^53 - 1 (encode larger exact
integers as strings). Params nesting is limited to 16 levels. These limits are
validated before submission instead of silently truncating data in firmware.
Console frames remain byte-oriented and have no JSON restriction.

## Validation status

- Host unit/integration tests cover codec corruption, fragmented I/O, request
  timeout without replay, stale responses, reset invalidation, and lib/HTTP ownership.
- Host duplex regression tests cover a backpressured peer requiring reads to
  unblock writes, escaped partial frames, replies during long input, console input
  during a slow HTTP/lib command, baud mismatch isolation, timeouts without replay,
  disconnect cleanup, and retention of running operations. This scheduling change
  has host validation only; the hardware measurements below predate it.
- Firmware builds with ESP-IDF 5.5.3; portable C codec passes 101 reference vectors.
- Real ESP32-S3: flash/verify, embedded lib negotiation/echo, stdout/stderr/ESP_LOG
  capture and application events verified. HTTP negotiation and 100 echo calls
  also exercised on hardware.
- Console input exposed a caller-stack overflow; console queue entries now use
  small dedicated chunks, and the demo task has a 4 KiB stack. The complete
  `gateway_smoke.py` suite passed on hardware after USB reconnection, including
  every input byte, events/logs arriving during a pending request, busy admission,
  application errors, timeout recovery, late-response isolation and no corrupt
  frames. CLI `app-call echo` also passed.

### Hardware measurements (2026-09-20)

macOS host and HTTP client on loopback, ESP32-S3 native USB Serial/JTAG,
ESP-IDF 5.5.3 gateway example. Latest run: 100 sequential echo requests.
These are end-to-end HTTP operation times with 2 ms result polling, including
client overhead; they are not wire-only timings or a real-time guarantee.

| Measurement | Result |
| --- | --- |
| Echo median | 3.34 ms |
| Echo P95 | 3.87 ms |
| Echo maximum | 5.76 ms |
| 500-line log burst command completion | 47.84 ms |
| Console chunks dropped during that burst | 54 |
| Control frames dropped during that burst | 0 |

The bounded console queue deliberately drops chunks under overload. The burst
result includes executing the application's 500 printf calls, not just queuing
its response. Local raw reports remain in ignored `.artifacts/gateway-smoke.json`.

### Remaining issue

After one flash during development, the native USB endpoint stopped delivering
bytes and ROM probing also failed. A physical USB reconnect restored operation;
only the test daemon held the port. The root cause is still unconfirmed. Do not
claim unattended flash-to-application recovery is reliable yet. Keep this as a
follow-up investigation separate from the now-passing application protocol tests.
