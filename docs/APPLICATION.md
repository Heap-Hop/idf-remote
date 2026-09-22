# Application protocol (experimental)

Console output, application commands, and events share one USB connection.
For firmware setup and supported hardware, see [Firmware](../firmware/README.md).

## Ownership and lifecycle

The per-device worker is the sole USB owner. Protocol/session code
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

The firmware has one TX owner, with bounded high-priority control and
lower-priority console queues.
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
protocol version, boot identity and supported methods. Request payload is
JSON {"method":"...","params":...}; response/error/event payloads are JSON.
Console payload is arbitrary bytes (including ANSI and partial lines).

Firmware stays in raw console mode until explicit hello. Connecting is an
opt-in operation for compatible firmware, not a probe sent to arbitrary REPLs.
Reset restores raw mode. ROM/panic/direct driver writes are outside stdio capture;
unframed bytes terminate a negotiated host session and are shown as raw output.
Attach the console once at startup, before application tasks/REPLs; direct USB
writers and independently redirected FILE streams are unsupported.

## Host interface

`POST /v1/application` **requires** the `Idempotency-Key` HTTP header for both
connect and command requests. Missing it returns HTTP 400 with
`{"error":"missing Idempotency-Key header"}`. The CLI supplies it automatically;
HTTP clients must generate a unique key per logical operation and reuse that key
with the same body when retrying that operation.

Request body:

```json
{
  "device_id": "DEVICE_ID",
  "monitor_baud": 115200,
  "timeout_ms": 2000,
  "command": {"method": "echo", "params": {"hello": "device"}}
}
```

For example, connect using curl (replace the device ID and use a fresh key for
each new connection attempt):

```sh
curl --fail-with-body http://127.0.0.1:38473/v1/application \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: connect-example-1' \
  -d '{"device_id":"DEVICE_ID","timeout_ms":2000}'
```

Omit `command` (or set it to null) to connect explicitly. The response is HTTP
202 with an ordinary Operation. Poll `/v1/operations/{id}` until completion;
`result` is handshake identity or the command's JSON response. Application
errors/timeouts produce failed operations; malformed requests fail admission.
Same-key retries reuse the retained operation rather than sending another
command. A new key is required for a new negotiation after disconnection; replaying
a completed connect operation does not reconnect. Reusing a key with different
request content returns 409. Retention is bounded. Busy devices return 409.

`/v1/events` and `/v1/stream` expose `application_connected`,
`application_disconnected`, `application_event`, `application_protocol_error`,
and decoded `raw`/`log` events. A command response is in its own
operation, not consumed from a shared subscription. Monitor stdin uses console
frames in an application session. Reset/flash invalidates the session before
opening the bootloader. USB reopen restores monitor transport, not an application
session: call connect again. Other clients must observe these lifecycle events.

The application session belongs to the daemon's device worker, not an individual
HTTP client. All subscribers for that device receive its lifecycle events,
including the client that initiated an operation. Explicit connect may replace
an existing session, but does **not** emit `application_disconnected` merely for
renegotiation. Its operation reports progress/failure; successful negotiation
broadcasts `application_connected`. Actual transport/session loss and hardware
operations that invalidate the session can still emit `application_disconnected`.
Track the connect operation to completion instead of waiting for a disconnect
notification to start another connect. Clients should coalesce reconnect attempts
and use bounded backoff after failures.

`Service::submit_application` is the embedded counterpart; `get_operation` and
`application_events` expose results and cursors without an HTTP listener.
These scheduling guarantees apply to the service through both lib and HTTP.
The low-level `application::Session` convenience methods remain synchronous;
embedded applications needing concurrent operations should use `Service`.
See [embedded_gateway.rs](../examples/embedded_gateway.rs) for Rust embedding.

## JSON profile

The firmware component uses cJSON. Strings/keys cannot contain embedded NUL;
use base64 for binary data. Numeric magnitude is limited to 2^53 - 1 (encode larger exact
integers as strings). Params nesting is limited to 16 levels. These limits are
validated before submission instead of silently truncating data in firmware.
Console frames remain byte-oriented and have no JSON restriction.

## Recovery limitations

Protocol negotiation is explicit; reconnect with `app-connect` after a daemon
restart, USB interruption, or firmware reset. Automatic unattended recovery from
flashing to an application session is not guaranteed. A native USB endpoint
that stops responding may require a physical reset or reconnect.
