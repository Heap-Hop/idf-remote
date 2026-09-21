# idf-remote

Remote firmware flashing, serial monitoring, and application control for ESP
boards. Build firmware where your tools run; connect the board to a host running
`idfr serve`. Clients access the hardware over HTTP.

This independent project is not affiliated with or endorsed by Espressif Systems.

## Build

With current stable Rust:

```sh
cargo build --release
```

The executable is `target/release/idfr` (`idfr.exe` on Windows). Add its directory
to `PATH` to use the commands below.

CI builds and tests macOS, Windows, and Linux. Hardware testing primarily covers
macOS with ESP32-S3; the CLI and application protocol are still evolving.

## Quick start

On the computer connected to the board:

```sh
idfr serve
```

The daemon listens on `127.0.0.1:9876` and discovers USB serial devices dynamically.
From another terminal, list devices and flash an ESP-IDF build:

```sh
idfr devices
idfr flash --build-dir build --monitor
```

With one device, selection is automatic. With multiple devices, use the ID from
`devices` or its current serial address:

```sh
idfr --device DEVICE_ID monitor
idfr --port SERIAL_PORT monitor
```

Replace `SERIAL_PORT` with a macOS, Linux, or Windows serial address. Use repeated
`--port` options on `serve` to restrict which devices the daemon manages.

## Flash and monitor

Flash imports images and offsets from ESP-IDF's `flasher_args.json`. Select an
image with `--image`, or provide a portable plan with `--plan`:

```sh
idfr plan --build-dir build
idfr flash --build-dir build --image app
idfr flash --plan plan.json --monitor
```

Empty manifest images are skipped with a warning. Before writing, the daemon
checks chip type, flash capacity, security state, and image ranges. Secure-boot
and flash-encryption provisioning are not supported.

```sh
idfr monitor
idfr reset --monitor
idfr probe
```

Monitor forwards keyboard input and displays colored logs. `Ctrl-]` exits;
`Ctrl-C` is sent to the device. Use `--no-input` for a read-only monitor. Monitoring
reconnects after USB interruptions, though early boot output can be missed.

Use `idfr --help` or `idfr COMMAND --help` for options, including serial writes,
flash reads, erasing, log capture, and JSON output.

## Remote access

For a remote USB host, forward its loopback listener over SSH:

```sh
ssh -N -L 9876:127.0.0.1:9876 user@usb-host
```

Clients then use the default URL, or an explicit `--url`:

```sh
idfr --url http://127.0.0.1:9876 devices
```

Non-loopback listeners require a token file on the host and clients:

```sh
idfr --token-file TOKEN_FILE serve --bind 0.0.0.0:9876
idfr --url http://USB_HOST:9876 --token-file TOKEN_FILE devices
```

HTTP is unencrypted; use SSH or a trusted encrypted network for remote traffic.

## Application gateway (experimental)

The optional [firmware component](firmware/README.md) carries console output,
JSON commands, and events over one ESP32-S3 USB Serial/JTAG connection. After
flashing the example firmware:

```sh
idfr app-connect
idfr app-call status
idfr monitor
```

Applications define their own commands. Rust applications can embed the service,
and remote clients can use its HTTP API. See [Application protocol](docs/APPLICATION.md)
for interfaces and session behavior. Ordinary flash and monitor use needs no
special firmware component.

## Development

See [Architecture](docs/ARCHITECTURE.md), [Testing](TESTING.md), and
[Agent guidance](AGENTS.md).

## AI assistance

This project has primarily been developed with AI assistance, and its code is reviewed and tested by the maintainer.

## License

[Apache-2.0](LICENSE).
