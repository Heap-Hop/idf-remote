# Optional application gateway

Use only with firmware known to implement the idf-remote gateway. Ordinary
flashing and raw serial monitoring do not need it. Do not send negotiation
bytes to an arbitrary console or REPL as a capability probe.

The documented firmware configuration is ESP-IDF 5.5.3 with ESP32-S3 native
USB Serial/JTAG. Other versions and transports require validation. Firmware
integration and example builds are documented in the
[firmware guide](https://github.com/Heap-Hop/idf-remote/blob/master/firmware/README.md).
Do not flash demo firmware merely to make an application call work.

## Connect and discover methods

Use the selected endpoint and authentication options with each command:

```sh
idfr --url http://HOST:PORT --device DEVICE_ID app-connect --json
```

Inspect the successful handshake's application identity, boot identity,
protocol version, and supported methods. Confirm these match the intended
firmware. Methods belong to the application; `status` and `echo` are demo
methods, not universal commands.

Call a supported method with parameters matching that application's contract:

```sh
idfr --url http://HOST:PORT --device DEVICE_ID app-call METHOD --params '{"key":"value"}' --timeout-ms 2000 --json
```

Determine a method's effects before calling it. A supported method name alone
does not authorize a state-changing action. `--timeout-ms` is milliseconds,
unlike the seconds used for log-capture `--timeout`.

The firmware JSON profile excludes embedded NUL in strings/keys, limits numeric
magnitude to 2^53 - 1, and permits parameter nesting up to 16 levels. Encode
larger exact integers as strings; use base64 for binary data.

## Sessions and concurrency

The session belongs to the daemon's device worker and is shared by clients.
Explicit connect can replace an existing session; coordinate with current work.
One application call and one console input can run concurrently. Another
request in an occupied lane receives `device_busy`; do not defeat this by
restarting the daemon or opening a second serial connection.

After negotiation, `idfr monitor` decodes console frames and frames its input.
An ordinary serial terminal cannot interpret that multiplexed stream. Keep the
daemon as the single USB owner.

Flash, reset, USB interruption, or daemon restart can invalidate the session.
Reopening USB for monitoring does not recreate it. Once the device is back and
the task requires gateway use, explicitly connect again and inspect the new
handshake before making a new call. Reusing a completed connect operation's
key does not negotiate a new session.

## Timeout and recovery

A timeout does not cancel the firmware method or prove that it did not run.
Query the known operation first. Do not reconnect and automatically replay the
method, especially when it changes state. Use application-specific readback
only when its semantics and authorization are known; otherwise report the
uncertain outcome and request the missing recovery decision.

The CLI supplies an idempotency key automatically. HTTP integrations must send
`Idempotency-Key` on `POST /v1/application`; a retained same-key, same-body
request returns the existing operation. Records are bounded and in memory,
so this is not persistent exactly-once execution. A fresh logical operation
needs a fresh key.

For HTTP payloads, events, and embedded Rust interfaces, consult the
[application protocol](https://github.com/Heap-Hop/idf-remote/blob/master/docs/APPLICATION.md).
