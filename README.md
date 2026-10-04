# idf-remote

Flash and monitor ESP boards remotely, separating your development and build
environment from the machine connected to the board.

For example:

- **Device A — USB host:** Connect the ESP board and run `idfr serve`, or use the
  [Android app](android/README.md). No ESP-IDF build toolchain is needed—just
  idf-remote. The USB host can be a PC, a Raspberry Pi, or even an Android phone.
- **Device B — development and build:** Build firmware with ESP-IDF, then run
  `idfr flash` or `idfr monitor`. This can be your development PC, a remote server,
  or even an isolated Docker environment.

The project grew out of the difficulty of accessing host USB devices from
containers and slow flashing over remote serial forwarding in our setup.
idf-remote transfers complete firmware images to Device A, then flashes them
locally over USB.

This independent project is not affiliated with or endorsed by Espressif Systems.

## Quick start

Install from crates.io on both computers using current stable Rust:

```sh
cargo install idf-remote --locked
```

For an Android USB host, follow the [Android setup](android/README.md) instead.

**On the USB host**, connect the board and start the daemon:

```sh
idfr serve
```

The daemon listens on `127.0.0.1:38473` and discovers USB serial devices dynamically.

**On the development machine**, open an SSH tunnel to the USB host and leave it
running:

```sh
ssh -N -L 38473:127.0.0.1:38473 user@usb-host
```

In another terminal, from an ESP-IDF project you have already built:

```sh
idfr devices
idfr flash --monitor
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

Flash imports images and offsets from `./build/flasher_args.json` by default.
Use `--build-dir PATH` for another build directory, `--image` to select images,
or `--plan` for a portable plan:

```sh
idfr plan
idfr flash --image app
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

The SSH tunnel above works with the default client URL. Use `--url` to select
another endpoint:

```sh
idfr --url http://HOST:PORT devices
```

Non-loopback listeners require a token file on the host and clients:

```sh
idfr --token-file TOKEN_FILE serve --bind 0.0.0.0:38473
idfr --url http://USB_HOST:38473 --token-file TOKEN_FILE devices
```

HTTP is unencrypted; use SSH or a trusted encrypted network for remote traffic.

## Android (experimental)

The [Android USB host app](android/README.md) supports flashing, console
monitoring, and application calls through Espressif native USB Serial/JTAG.
See its README for installation, USB permissions, and remote access setup.

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

Build from source with current stable Rust:

```sh
cargo build --locked --release
```

The executable is `target/release/idfr` (`idfr.exe` on Windows).
CI builds and tests macOS, Windows, and Linux. Hardware testing primarily covers
macOS with ESP32-S3; the CLI and application protocol are still evolving.

See [Architecture](docs/ARCHITECTURE.md), [Testing](TESTING.md), and
[Agent guidance](AGENTS.md).

## AI assistance

This project has primarily been developed with AI assistance, and its code is reviewed and tested by the maintainer.

## License

[Apache-2.0](LICENSE).
