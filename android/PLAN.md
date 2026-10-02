# Android implementation plan

Branch: `feat/android-usb-host`. Keep changes local until user hardware acceptance;
then prepare a PR. The separately authorized espflash fork is published under
Heap-Hop on `feat/android-transport`; idf-remote remains local until acceptance.

## Library and sample

- [x] Preserve desktop defaults; gate desktop dependencies and CLI.
- [x] Android fd backend with exclusive sessions and attachment-scoped IDs.
- [x] Reuse Service/device worker for monitor, console input and application calls.
- [x] App-private artifact storage; JNI adapter with orderly shutdown.
- [x] Foreground sample with USB permission, log view, JSON calls and loopback HTTP.
- [x] ARM64 Rust library and debug APK build.
- [x] Integrated phone validation: monitor, console echo, application connect/status/echo.
- [x] Shared ownership through UI/library and forwarded HTTP: UI Status reused the HTTP-negotiated session.
- [x] Three explicit close/open cycles with fresh attachment IDs and successful application renegotiation.
- [ ] Physical detach/replug and permission-denial testing of the integrated sample.
- [ ] User acceptance before PR creation.

## Flash milestone

- [x] Publish the opt-in transport seam on Heap-Hop/espflash, based on v4.5.0.
- [x] Bootloader entry + SYNC and chip/capacity/MAC probe on the phone.
- [x] Native USB IO and atomic CDC modem controls; desktop retains crates.io espflash.
- [x] Share desktop image/capacity/security validation; write/verify and recover application calls.
- [x] Read back 64 KiB and compare byte-for-byte with the application image.
- [ ] Hardware erase validation and additional Android/board coverage.

## Later

- Foreground Android service/background lifecycle.
- Automatic reconnect and stable identity policy.
- Additional USB-UART drivers and ABI/device coverage.

## Validation evidence

- ARM64 Android library + APK build; Android lint: 0 errors, 11 sample/tooling warnings.
- Desktop regression suite and no-default-feature library tests passed; host and Android clippy passed.
- Wireless ADB forwards the phone's loopback service to a separate desktop port.
- HTTP status/echo and plain + multiplexed console input passed on gateway-demo.
- During a 3000 ms application callback, console input echoed in approximately 166 ms over wireless ADB; callback subsequently returned 3000. This is an end-to-end observation, not a USB-only benchmark.
- A deliberately shorter client timeout failed without replay, as expected.
- No firmware reboot during reconnects (boot identity remained unchanged).
- Native USB probe reports the expected ESP32-S3 and 4 MiB flash.
- Bootloader, partition table and application upload/write/verify passed over wireless ADB HTTP forwarding; logs resumed after reset.
- A 64 KiB application readback matched the uploaded image exactly.
- Post-flash/readback application hello, status and JSON echo passed.
- Fixed a real USB receive starvation issue: keeping multiple IN transfers queued during command writes/waits allows all burst ROM SYNC responses to arrive.
- Fork revision is pinned in both lockfiles; desktop dependency tree retains registry espflash.
- Physical USB unplug/replug, permission denial, background lifecycle and full-chip erase remain unverified as listed above.
