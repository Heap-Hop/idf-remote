# Application gateway MVP

Goal: one USB owner supports remote firmware updates and local/HTTP application
control, with console output and application events on the same connection.

## Incremental plan

- [ ] Versioned framing, bounded decoder and corruption tests.
- [ ] Optional ESP-IDF component and ESP32-S3 USB Serial/JTAG example.
- [ ] Host application session library, worker integration, HTTP and CLI.
- [ ] Real board: flash, negotiate, requests, events and stdio concurrently.
- [ ] Record measurements, limits and reproducible usage.

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

MVP serializes application requests per device (other calls receive device_busy),
while console and events continue flowing during the request. The firmware has
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
