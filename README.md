# idf-remote

Remote flash and serial monitor for ESP-IDF projects.

`idf-remote` is a host daemon and CLI for using ESP development boards over
HTTP. The daemon owns the physical serial ports; local or remote clients upload
firmware, start hardware operations, and stream monitor output.

This is an independent project and is not affiliated with or endorsed by
Espressif Systems.

The current desktop implementation supports:

- dynamic discovery of multiple USB serial devices;
- flash plans and ESP-IDF `flasher_args.json` imports;
- probe, flash, erase, read, reset, serial write, and interactive monitor;
- one serialized worker per device, so independent boards can run concurrently;
- stable operation IDs, retry keys, progress events, and bounded log cursors;
- passive monitor recovery after USB or serial interruptions.

The protocol and CLI are still evolving. macOS with ESP32-S3 hardware is the
primary tested path; CI builds and tests macOS, Windows, and Linux.

## Build

Install the current stable Rust toolchain, then run:

```sh
cargo build --release --locked
```

The binary is written to `target/release/idf-remote`.

## Quick start

Start the daemon on the computer connected to the boards:

```sh
idf-remote serve
```

The default listener is `127.0.0.1:9876`. With no `--port` arguments, the daemon
discovers supported USB serial devices and follows attach/detach events.

List devices from another terminal:

```sh
idf-remote devices
idf-remote --json devices
```

When the daemon exposes one device, hardware commands select it automatically.
With multiple devices, select one by the opaque ID shown by `devices`, or by its
current host address:

```sh
idf-remote --device dev_0123456789abcdef01234567 monitor
idf-remote --port /dev/cu.usbmodemXXXX monitor
idf-remote --port COM5 monitor
```

`DeviceId` is the API identity. A serial path is only a current transport
locator and may change after reconnecting.

## Flash an ESP-IDF build

Validate and normalize the build artifacts without contacting hardware:

```sh
idf-remote plan --build-dir build
```

Flash every non-empty image described by `build/flasher_args.json`, then enter
the interactive monitor:

```sh
idf-remote flash --build-dir build --monitor
```

Flash selected named images while preserving their manifest offsets:

```sh
idf-remote flash --build-dir build --image app
idf-remote flash --build-dir build --image bootloader --image app
```

Empty entries in the ESP-IDF manifest are skipped with a warning. `--image`
remains available for selecting an explicit subset.

Before writing, the client validates and snapshots every artifact. The daemon
checks upload digests, enters the ROM loader, confirms the chip type, reads the
security state and detected flash capacity, then validates all segment ranges.
Secure boot and flash-encryption provisioning are currently rejected.

For a toolchain-independent workflow, pass `--plan path/to/plan.json`. Artifact
paths in the plan are resolved relative to that file:

```json
{
  "version": 1,
  "chip": "esp32s3",
  "flash_settings": {
    "mode": "dio",
    "frequency": "80m",
    "size": "4MB"
  },
  "segments": [
    { "offset": "0x0", "file": "bootloader.bin" },
    { "offset": "0x10000", "file": "app.bin" }
  ]
}
```

## Monitor and other operations

```sh
# Interactive monitor; Ctrl-] exits and Ctrl-C is sent to the device.
idf-remote monitor

# Reset first, wait for a startup marker, and save exact serial bytes.
idf-remote reset --monitor --wait 'READY' --timeout 30 \
  --raw-log startup.serial

# Probe the ROM loader and report chip/flash information.
idf-remote probe

# Send text without resetting the board.
idf-remote serial-write --text restart --newline

# Read flash without overwriting an existing output file.
idf-remote read-flash --offset 0 --size 4096 --output first-sector.bin

# Full erase requires the exact confirmation value.
idf-remote erase-flash --confirm erase-all-flash
```

Monitor output uses ESP-IDF-style level colors when stdout is a terminal. Use
`--color always` or `--color never` to override detection, and `--no-input` for
a read-only monitor.

## Remote clients and authentication

Point a client at another daemon with `--url`:

```sh
idf-remote --url http://127.0.0.1:9876 devices
```

An SSH tunnel or private network can keep the daemon bound to loopback. If it
must listen on a non-loopback address, a non-empty token file is mandatory:

```sh
# Host
idf-remote --token-file /path/to/token serve --bind 0.0.0.0:9876

# Client
idf-remote --url http://HOST:9876 --token-file /path/to/token devices
```

The service currently uses plain HTTP. Protect remote traffic with SSH, a VPN,
or another trusted encrypted transport.

## Design and testing

See [Architecture](docs/ARCHITECTURE.md) for the component boundaries and
[Testing](TESTING.md) for software and opt-in hardware checks.

## License

Licensed under the [Apache License 2.0](LICENSE).
