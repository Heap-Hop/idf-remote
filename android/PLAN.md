# Android implementation plan

Branch: `feat/android-usb-host`. Keep changes local until user hardware acceptance;
then prepare a PR. Do not publish an espflash fork as part of this first slice.

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

## Flash milestone (still pending)

- [ ] Design the espflash transport seam and review upstream/fork strategy.
- [ ] Bootloader entry + SYNC without flash writes.
- [ ] Adapt serial IO and atomic control lines while retaining desktop behavior.
- [ ] Validate probe, flash size/security checks, write/verify, and application recovery.

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
- Unsupported probe returned HTTP 400 before bootloader access.
- Physical USB unplug/replug, permission denial, background lifecycle and Android flashing remain unverified/unimplemented as listed above.
