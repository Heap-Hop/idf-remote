# Android implementation plan

Branch: `feat/android-usb-host`, targeting `master`. User hardware acceptance is
complete; the Android MVP is ready for PR review. The espflash fork is published
under Heap-Hop on `feat/android-transport` and pinned by revision.

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
- [x] Physical detach/replug and default-app automatic reconnect of the integrated sample.
- [ ] Permission-denial testing of the integrated sample.
- [x] User hardware acceptance before PR creation.

## Flash milestone

- [x] Publish the opt-in transport seam on Heap-Hop/espflash, based on v4.5.0.
- [x] Bootloader entry + SYNC and chip/capacity/MAC probe on the phone.
- [x] Native USB IO and atomic CDC modem controls; desktop retains crates.io espflash.
- [x] Share desktop image/capacity/security validation; write/verify and recover application calls.
- [x] Read back 64 KiB and compare byte-for-byte with the application image.
- [ ] Hardware erase validation and additional Android/board coverage.

## LAN and display milestone

- [x] Loopback default and opt-in IPv4 LAN listener with required Bearer token.
- [x] Configurable port, persistent random token, and URL/token clipboard buttons.
- [x] Wi-Fi/Ethernet address display and explicit refresh; exclude cellular addresses.
- [x] Persistent screen-on switch using the Activity window flag.
- [x] Native listener-policy tests and shared router authentication regression.
- [x] Phone verification: URL/token copy-paste, flag on/off, settings preserved across APK update.
- [x] PC direct Wi-Fi verification: unauthorized rejection, authenticated devices, application echo and logs.

## Later

- Foreground Android service/background lifecycle.
- Stable identity policy and recovery beyond system-authorized USB attach.
- Additional USB-UART drivers and ABI/device coverage.

## Validation evidence

- ARM64 Android library + APK build; Android lint: 0 errors, 20 sample/tooling warnings.
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
- Permission denial, background lifecycle and full-chip erase remain unverified as listed above.

## USB defaults and console follow

- [x] Declare the native USB Serial/JTAG attach filter and system default-app handler.
- [x] Handle cold-start and existing-Activity attach intents; check current enumeration and system permission before automatic connection.
- [x] Avoid reopening an active handle on duplicate attach events or switching devices implicitly.
- [x] Follow new console output; pause for manual scrollback and provide Latest logs to resume.
- [x] Test phone saved a default-app association including the board USB serial; system launch connected automatically.
- [x] User-confirmed second physical replug restored logs; host verified automatic reconnect and authenticated LAN discovery.
- [x] Phone UI verification: new logs follow the tail, manual scrollback stays fixed during output, and Latest logs resumes following. APK build and lint passed.

## PR review corrections

- [x] Preserve device identity and a unique request token in immutable USB permission callbacks; recheck live permission and ignore stale callbacks.
- [x] Render raw console bytes incrementally with attachment-local UTF-8 state, without duplicate log records.
- [x] Accept arbitrary JSON application events; skip malformed events without terminating the reader.
- [x] Separate application waits from console input with bounded application admission and attachment-scoped JNI leases.
- [x] Explicitly join native workers before closing Java USB, even when another caller retains a gateway reference.
- [x] Five JVM regression tests, APK build, native Android check/clippy and lint passed. UI tests and lint now run in CI.
- [x] Phone UI: Tab and Ctrl-C echoes arrived more than two seconds before delay(2500) completed. Disconnect/reconnect during a delayed call allocated a fresh attachment and recovered application negotiation without displaying the old result.
- [ ] Fresh manual permission grant/denial after the callback correction; the connected test board already had permission.

## Proposed next milestone: separate HTTP and USB lifetimes

Assessment: medium scope, localized lifecycle work rather than a transport or
protocol rewrite. Recommended before adding an Android foreground service.

Current coupling: JNI open(fd) creates the backend, Service, listener and one
selected device. USB detach calls close(handle), destroying all of them.

Proposed sequence:

1. Split JNI into start/stop gateway and attach/detach USB; the app starts the
   authenticated listener independently of having a connected board.
2. Add app-private storage support to Service's existing dynamic discovery
   constructor. It already supports an empty inventory and later devices.
3. Make Android detach surface an explicit disconnected state and fail pending
   work without replay. Join/release the affected USB worker before closing the
   Java connection, while keeping HTTP and unrelated devices alive.
4. Bound retired attachment records/workers. The current backend allocates fresh
   IDs per attachment, while discovery retains old records and has a 64-device
   limit; repeated reconnects must not exhaust that limit. Keep stable physical
   pairing a separate policy, never silently redirect an old ID to new hardware.
5. Replace per-device JNI event polling with gateway/device selection and
   separate HTTP running, USB connected and application-session UI states.

Acceptance: start with zero USB devices; devices endpoint stays reachable during
unplug; authorized reattach works without restarting HTTP; in-flight operations
fail clearly and are not replayed; repeated attach/detach cycles do not leak
workers/fds or exhaust inventory. Existing flash/monitor/app-call APIs remain
compatible. Background service work remains a later milestone.
