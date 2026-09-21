# ESP application gateway (experimental)

This optional component lets a local/remote host exchange application commands
and events while observing normal console output on one USB connection.
Test baseline: **ESP-IDF 5.5.3**, **ESP32-S3 native USB Serial/JTAG**.
This is not the USB-OTG/TinyUSB transport or an Android implementation.

## Build and run the example

Activate ESP-IDF 5.5.3, then from this repository:

```sh
idf.py -C firmware/examples/gateway build
```

Start `idfr serve` on the USB host and inspect `idfr devices`. Use
only a disposable board; the next command overwrites its application:

```sh
idfr --port SERIAL_PORT flash --build-dir firmware/examples/gateway/build
idfr --port SERIAL_PORT app-connect
idfr --port SERIAL_PORT app-call echo --params '{"hello":"device"}'
idfr --port SERIAL_PORT app-call status
idfr --port SERIAL_PORT monitor
```

A board placed in download mode by its BOOT button may need a physical RESET
(with BOOT released) after its first flash. Connect only after the application
boots. Before connect, output is ordinary text. After connect, an ordinary
serial monitor needs a device reset to get plain text again; `idfr monitor`
uses decoded console bytes and frames its keyboard input automatically.

The example emits ESP_LOGI, printf, stderr and a `tick` event each second. Its
stdin task prints each received byte as `stdin:XX`; it is deliberately not a
full REPL. Methods: `echo` returns JSON params; `status` reports uptime and queue
drops; `log_burst` prints 500 lines; `delay` waits the specified 0..5000 ms.

## Firmware integration

Add `firmware/components/idf_remote` to your component search path and include
`idf_remote.h`. At the start of `app_main`, before application console tasks:

```c
idf_remote_config_t config = {
    .application = "my-device",
    .version = "0.1.0",
    .methods_json = "[\"status\"]",
    .handler = handle_command,
    .context = NULL,
};
ESP_ERROR_CHECK(idf_remote_attach_console(&config));
```

Attachment is local and returns without waiting for a host. It installs the USB
driver and redirects the shared default stdin/stdout/stderr FILE objects through
VFS. ESP_LOG levels remain controlled by the application. A dedicated command
task invokes the callback with method and JSON params; return a cJSON result
whose ownership transfers to the component. `idf_remote_publish` copies an event
into a bounded output queue; callers keep ownership of their cJSON object.

## Scope and limits

- Initialization is once per boot with process-lifetime resources; attach failure
  is a startup failure. No detach/uninstall or arbitrary late takeover of a REPL.
- Do not preinstall the USB driver or write directly to it after attach. Default
  shared FILE streams are captured; independently redirected FILE streams, ROM,
  early/panic output and low-level driver writes are outside capture.
- Console VFS implements read/write/fstat/fcntl, not select/termios. Input is
  delivered as bytes; your application implements a REPL if needed. Output
  follows IDF's configured stdout newline conversion.
- JSON strings/keys cannot contain NUL, numbers must have magnitude <= 2^53 - 1,
  and params nesting is limited to 16 levels. Encode binary/large integers as strings.
- Frames carry at most 1024 payload bytes. Console chunks are about 192 bytes.
  Queues: 16 console frames, 8 control/event frames, 4 commands, 1024 input bytes.
  Console writes do not block on USB; overload drops chunks. Inspect counters.
- Control/event frames are selected before console frames. This bounds queued
  log work, but cannot preempt USB bytes or a running application callback.
  Do not use this protocol for safety-critical real-time control.
- Reset/flash ends a session; reconnect explicitly. No application command is
  automatically replayed. A command may still execute after its host times out.
- Version/API/wire format are experimental. Other IDF versions and transports
  need validation before declaring support.

## Embedded host use

`src/mux.rs` handles framing; `src/application.rs` provides synchronous sessions
on a caller-owned `SerialIo`. The per-device worker uses those same modules.
For direct Rust embedding without an HTTP listener:

```sh
# Stop the daemon that owns this port first.
cargo run --example embedded_gateway -- SERIAL_PORT
```

See `examples/embedded_gateway.rs` for the shared service API and event cursor.
An embedded app and a separate daemon must not both open the same port.
