# Architecture

## Boundary

`idf-remote` separates an ESP build environment from the computer that owns the
physical USB connection:

```text
build tools / client
        |
        | HTTP
        v
idf-remote daemon
        |
        | desktop serial transport
        v
     ESP device
```

The client reads local artifacts and sends bytes plus validated metadata. The
daemon never depends on the client's filesystem or ESP-IDF installation.

## Device model

Public operations use an opaque `DeviceId`. A `DeviceDescriptor` separately
reports the display name, capabilities, availability, activity, and transport
metadata such as the current serial path.

The path is a locator, not an identity. On desktop systems a reconnect may
change it, and macOS may expose `cu.*` and `tty.*` aliases for the same interface.
The registry groups aliases and only migrates a locator when a strong USB
identity has one unambiguous match.

Device discovery is dynamic. A daemon can start with no boards, create a worker
when a supported device appears, mark it disconnected when removed, and resume
monitoring after it returns. Passing one or more `--port` values to `serve`
enables a restricted compatibility mode instead.

The registry is currently in memory. Device IDs and operation records are not a
persistent pairing or access-control mechanism across daemon restarts.

## Operations and concurrency

Each managed device has one worker. That worker serializes probe, flash, erase,
read, reset, monitor, and serial-write access for its port. Separate device
workers can make progress concurrently. Busy or unsupported requests fail at
admission rather than racing for the same hardware.

HTTP handlers validate requests and enqueue bounded work. Blocking serial and
flash operations run outside the asynchronous HTTP executor. Operations expose
status, structured events, progress, and a cursor into a bounded per-device log
buffer. Clients may poll, long-poll, or consume server-sent events.

A request key makes retries idempotent while its operation remains in the
daemon's in-memory store. Reusing a key with different request content is
rejected.

## Flash flow

The client imports either a portable `FlashPlan` or an ESP-IDF
`flasher_args.json`. It resolves image paths locally, rejects invalid ranges and
overlapping erase sectors, snapshots the files, and computes their digests.

The server verifies the upload and plan before opening hardware. The device
worker then:

1. enters the ROM bootloader;
2. confirms the detected chip matches the plan;
3. rejects unsupported secure-boot or flash-encryption provisioning;
4. reads the physical flash capacity and validates every segment;
5. writes and verifies the images while publishing progress;
6. hands the same connection to the monitor when requested.

An operation error after writing begins may leave a partial image. The failure
does not automatically boot that image.

## Monitor lifecycle

The monitor keeps the serial connection inside the device worker. Interactive
input is sent through a separate HTTP request and returns to that same worker,
so no second process opens the port. A short read timeout bounds keyboard-input
latency.

Broken or missing serial endpoints move the device into `reconnecting`. The
worker retries passive reopen with bounded backoff and publishes reconnect state
events. A USB device can emit early boot bytes before an operating system
recreates its serial endpoint; those bytes cannot be recovered by the daemon.

## Transport boundary

Core request models refer to device IDs, capabilities, plans, and byte streams.
Desktop serial details live behind `DeviceBackend`, `DeviceSession`, and
`SerialIo` traits. This keeps HTTP, operation scheduling, validation, and log
handling independent from `serialport` and `espflash` connection objects.

A future Android USB Host backend can implement the same boundary with Android
USB APIs while reusing the protocol and scheduling layers. Android support is a
design direction, not a current feature.

## Network security

The daemon defaults to loopback without authentication. Binding a non-loopback
address requires a Bearer token loaded from a file. The protocol is plain HTTP,
so remote deployments should add encryption and peer access control through an
SSH tunnel, VPN, or equivalent trusted transport.

The token protects daemon access; it does not turn a serial path or `DeviceId`
into proof of physical device ownership. Applications that automate destructive
work should inspect the descriptor and probe result, and should supply their own
expected-device policy.
