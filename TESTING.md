# Testing

## Software checks

These checks use fake devices and synthetic artifacts. They do not open serial
ports or modify hardware.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

CI runs the same checks on macOS, Windows, and Linux.

## ESP32-S3 fixture

The optional fixture in `tests/firmware` prints `IDF_REMOTE_BOOT smoke-v1` at
startup and emits `IDF_REMOTE_TICK n` once per second. Build it in an existing
ESP-IDF environment:

```sh
cd tests/firmware
idf.py set-target esp32s3
idf.py build
```

Start the daemon from the repository root:

```sh
cargo build
target/debug/idf-remote serve
```

Inspect the dynamically discovered device before any destructive test:

```sh
target/debug/idf-remote devices
target/debug/idf-remote --port /dev/cu.usbmodemXXXX probe --json
```

Replace the example path with the dedicated test board. On Windows, use its
`COM` address. The full hardware smoke test requires both the current address
and expected USB serial so it can reject the wrong device before programming:

```sh
mkdir -p .artifacts
python3 tests/hardware_smoke.py \
  --port /dev/cu.usbmodemXXXX \
  --usb-serial EXPECTED_SERIAL \
  --build-dir tests/firmware/build \
  --report .artifacts/hardware-smoke.json
```

This test flashes the fixture, verifies startup, exercises conflict and error
contracts, checks that a mismatched target chip fails before writing, and resets
the board. Reports and raw logs under `.artifacts` are ignored by Git.

## Discovery and reconnect checks

The following scripts are opt-in physical tests. They do not flash or erase:

- `tests/reconnect_smoke.py` checks detach/reattach recovery for one device.
- `tests/dynamic_discovery_smoke.py` checks hot-add while the daemon is running.
- `tests/multi_device_smoke.py` checks isolation between two devices.

Each script accepts `--help` and requires expected USB serials or markers. Use
only boards that can be identified unambiguously.

## Monitor latency

`tests/monitor_latency.py` measures byte echo latency through the HTTP monitor
path. It can use a pseudo-terminal for deterministic host-only measurements or
the fixture in `tests/latency_firmware` for an opt-in USB measurement:

```sh
python3 tests/monitor_latency.py --help
```

## Destructive erase check

Only use a disposable, positively identified board. Validate the recovery image
before erasing:

```sh
target/debug/idf-remote plan --build-dir tests/firmware/build
target/debug/idf-remote devices
target/debug/idf-remote --port /dev/cu.usbmodemXXXX \
  erase-flash --confirm erase-all-flash
```

Restore known firmware immediately after the test.
