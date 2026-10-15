# ELF source fixtures

These original Apache-2.0 fixtures exercise three project types on ESP32-S3.
They default to UART console output at 115200 baud, with an optional native USB
Serial/JTAG console profile. They use no Wi-Fi or external sensors.
They do not include the optional application gateway.

| Project | Source | Startup marker | Heartbeat |
| --- | --- | --- | --- |
| ESP-IDF C | `firmware` | `IDF_REMOTE_BOOT smoke-v1` | `IDF_REMOTE_TICK` |
| Rust no_std / esp-hal | `firmware-rust-nostd` | `IDFR_NOSTD_BOOT smoke-v1` | `IDFR_NOSTD_TICK` |
| Rust std / esp-idf-sys | `firmware-rust-std` | `IDFR_STD_BOOT smoke-v1` | `IDFR_STD_TICK` |

The std fixture uses a custom factory partition at `0x20000`, proving that its
own build artifacts are used instead of an assumed default layout. The no_std
fixture includes an ESP-IDF application descriptor. Neither fixture's compilation
or byte comparison establishes that firmware boots on physical hardware.

## Reproduce software checks

Install current stable Rust for idfr. Firmware fixtures use the separate Xtensa
Rust 1.90.0.0 toolchain (`espup --toolchain-version 1.90.0`). Keep their Cargo.lock
files: notably esp-rom-sys 0.1.3 avoids newer patches requiring Rust 1.95.
Native std builds use ESP-IDF v5.4.2, selected in the fixture config.

From the repository root, on Linux with CMake, Ninja, Python venv support, a C
compiler, libudev and libclang development packages, and the libusb runtime:

```sh
cargo install espflash --version 4.5.0 --locked
cargo install espup --version 0.18.0 --locked
cargo install ldproxy --version 0.3.4 --locked
mkdir -p .artifacts
espup install --targets esp32s3 --toolchain-version 1.90.0 --export-file "$PWD/.artifacts/esp-export.sh"
source .artifacts/esp-export.sh
cargo build --locked --release
python3 tests/run_elf_fixtures.py --idfr target/release/idfr --espflash ESPFLASH_EXECUTABLE --output .artifacts/real-fixtures
```

Use an absolute path for `ESPFLASH_EXECUTABLE`. The output directory must be new.
The script builds with `--locked`, records build logs, and obtains the std
bootloader and partition table from esp-idf-sys's Cargo build-script message.
This discovery belongs only to the test harness; idfr itself remains Cargo-free.
Each Rust project is compared against espflash in six scenarios: baseline,
explicit binary support images, CSV partitions, a selected application partition,
app-only, and config/CLI overrides. Reports include input and output digests.

The std build prepares native ESP-IDF tools under `.artifacts/.embuild/espressif`.
Reuse them for the C regression:

```sh
export IDF_TOOLS_PATH="$PWD/.artifacts/.embuild/espressif"
python3 "$IDF_TOOLS_PATH/esp-idf/v5.4.2/tools/idf_tools.py" install --targets esp32s3
source "$IDF_TOOLS_PATH/esp-idf/v5.4.2/export.sh"
idf.py -C tests/firmware -B "$PWD/.artifacts/firmware-c" -D SDKCONFIG="$PWD/.artifacts/firmware-c/sdkconfig" -D IDF_TARGET=esp32s3 build
python3 tests/check_idf_fixture.py --idfr target/release/idfr --build-dir .artifacts/firmware-c --output .artifacts/real-fixtures/c.json
```

Generated binaries and logs stay out of git. The manual `ELF source fixtures`
workflow executes the same builds and checks. Ordinary Rust CI uses a synthetic
ELF generated from source plus an independent espflash comparison on Linux;
no firmware toolchain or serial device is needed for ordinary `cargo test`.

## Native USB Serial/JTAG console

For boards connected through the ESP32-S3 native USB Serial/JTAG port, pass
`--usb-jtag` to `tests/run_elf_fixtures.py`. The script builds both Rust fixtures
in separate target directories and runs the same byte comparisons. The no_std
fixture selects `console-usb-jtag` with default features disabled; the std
fixture applies `sdkconfig.usb-jtag` in addition to its defaults.

Build the C fixture with the same console profile in a fresh build directory:

```sh
idf.py -C tests/firmware -B "$PWD/.artifacts/firmware-c-usb-jtag" -D SDKCONFIG="$PWD/.artifacts/firmware-c-usb-jtag/sdkconfig" -D 'SDKCONFIG_DEFAULTS=sdkconfig.defaults;sdkconfig.usb-jtag' -D IDF_TARGET=esp32s3 build
```

Choose the console profile that matches the physical connection. UART and native
USB builds use the same startup and heartbeat markers.

## Optional hardware validation

Only after authorization, verify the test board's current address and expected
identity. Use a plan from the matching baseline case; app-only requires compatible
supporting images already on the board. Flash with the matching startup marker:

```sh
idfr --device DEVICE_ID flash --plan PLAN_FILE --wait 'EXPECTED_BOOT_MARKER' --timeout 30
idfr --device DEVICE_ID monitor --no-input --wait 'EXPECTED_TICK_MARKER' --timeout 10
```

Record write/verify success, startup marker, heartbeat, tool versions, board and
transport separately from software results. Keep hardware reports in `.artifacts/`.
