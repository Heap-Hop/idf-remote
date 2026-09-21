# Testing

## Software checks

CI runs these checks on macOS, Windows, and Linux. They use fake devices and
synthetic artifacts, without opening physical serial ports:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

`--locked` prevents dependency resolution from changing `Cargo.lock` during
verification. It is optional for normal local builds.

For firmware codec changes, compare the C implementation against reference
vectors (requires Python 3 and a C compiler):

```sh
python3 tests/test_mux_codec.py
```

## Hardware tests

Use a dedicated test board. Identify its current port and expected USB serial
before running a script. Some tests flash or reset the board; do not run another
daemon or serial monitor against the same port. Keep reports in `.artifacts/`.

Build the basic ESP32-S3 fixture in an activated ESP-IDF environment:

```sh
idf.py -C tests/firmware set-target esp32s3
idf.py -C tests/firmware build
```

Start `idfr serve`, inspect `idfr devices --json`, then run:

```sh
mkdir -p .artifacts
python3 tests/hardware_smoke.py \
  --port SERIAL_PORT --usb-serial EXPECTED_SERIAL \
  --build-dir tests/firmware/build \
  --report .artifacts/hardware-smoke.json
```

This flashes the fixture and checks boot output, operation contracts, and target
validation. The scripts below provide additional opt-in checks; run each with
`--help` for required device identities, markers, and output paths.

| Script | Purpose |
| --- | --- |
| `tests/reconnect_smoke.py` | Detach/reattach recovery |
| `tests/dynamic_discovery_smoke.py` | Device discovery while the daemon runs |
| `tests/multi_device_smoke.py` | Isolation between devices |
| `tests/monitor_latency.py` | Echo latency, using `tests/latency_firmware` |
| `tests/gateway_smoke.py` | Application calls, events, stdio, timeouts, and overload |

For a full erase test, validate a recovery image first, select the board
explicitly, and restore known firmware afterward. See `idfr erase-flash --help`.

## Application gateway

Build and flash the [gateway example](firmware/README.md), then start the daemon
and run:

```sh
python3 tests/gateway_smoke.py --url http://127.0.0.1:38473 \
  --port SERIAL_PORT --usb-serial EXPECTED_SERIAL \
  --report .artifacts/gateway-smoke.json
```

The script requires the gateway demo firmware and does not flash it. Use an
explicit `--url` when a test script's default differs from your daemon address.
Report hardware results separately from host-only tests, with the tested board,
transport, and ESP-IDF version.
