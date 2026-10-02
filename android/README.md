# Android USB host (experimental)

This sample embeds the `idf_remote` Rust library. Android grants USB permission
and supplies an owned duplicate of the device fd. `AndroidUsbBackend` feeds the
same device worker, console decoder, application protocol and HTTP router used
on desktop. Kotlin never performs USB transfers.

## Current scope

- ARM64 Android 8+ with USB Host; Espressif native USB Serial/JTAG `303a:1001`.
- Console monitoring/input, application connect/calls/events, and backend reset.
- Local library API and HTTP access share one USB owner and admission rules.
- Loopback by default; optional IPv4 LAN listener with a generated Bearer token.
  HTTP / Display offers configurable port, copy URL/token, and a screen-on switch.
- Probe, flash, read-flash and erase-flash use a pinned espflash transport fork,
  with the same image/capacity/security validation as desktop. Desktop defaults
  continue to use crates.io espflash.
- Foreground sample, one selected USB device. Replug requires Connect and Android
  permission if prompted. Application negotiation is explicit; commands are never
  replayed automatically. No background service or persistent device pairing yet.
- Log view shows complete decoded lines; it is not an ANSI terminal emulator.

## Build

Use JDK 17, Android SDK 35, NDK 28.2.13676358, and stable Rust:

```sh
rustup target add aarch64-linux-android
cd android
./scripts/build.sh
```

Set `ANDROID_HOME` / `ANDROID_NDK_HOME` if needed. The build script rebuilds native
code before Gradle; running Gradle alone does not. APK:
`app/build/outputs/apk/debug/app-debug.apk`.

## Phone test

Pair the phone using Android 11+ Wireless debugging, then install over Wi-Fi:

```sh
adb pair PHONE_IP:PAIRING_PORT
adb connect PHONE_IP:DEBUG_PORT
adb -s PHONE_SERIAL install -r app/build/outputs/apk/debug/app-debug.apk
adb -s PHONE_SERIAL shell am start -n dev.idfremote.android/.MainActivity
```

Stop any other app using the same USB device. Select the board, grant permission,
and observe logs. With the [gateway firmware](../firmware/README.md), Send echoes
input bytes; App connect negotiates multiplexing, Status/Echo invoke application
methods. Console input remains available after negotiation. Reconnecting the
USB handle does not reset firmware: use App connect again to recover its session.

## LAN access and display

Open **HTTP / Display** before connecting USB. Enable **LAN access**, choose the
port (default `38473`), and tap **Save**. Then connect the board. While connected,
network settings are locked; disconnect first to change them. URLs are refreshed
from Wi-Fi/Ethernet addresses when opening settings or tapping Refresh addresses.

Use **Copy URL** and **Copy token** to transfer the connection details to the PC.
The generated token is stored privately on the phone and survives app restarts.
**New token** replaces it while disconnected. Save the copied token in a local
file and use the standard CLI:

```sh
umask 077
printf '%s' 'PASTE_TOKEN' > idfr-token
idfr --url http://PHONE_IP:38473 --token-file idfr-token devices
idfr --url http://PHONE_IP:38473 --token-file idfr-token flash --build-dir ./build --monitor
idfr --url http://PHONE_IP:38473 --token-file idfr-token app-connect
idfr --url http://PHONE_IP:38473 --token-file idfr-token app-call status
```

LAN mode listens on `0.0.0.0` and requires the token for every HTTP endpoint,
including loopback access. HTTP is unencrypted; use a trusted network. Token
clipboard content is marked sensitive to suppress system previews.

**Keep screen on while app is visible** takes effect immediately and is saved
independently of network settings. It defaults to on. Keep the app visible during
use; this does not provide a background/foreground service or guarantee operation
after locking the phone. Foreground service support is a later milestone.

With LAN access off, the listener binds `127.0.0.1` without a token. Wireless ADB
forwarding remains available:

```sh
adb -s PHONE_SERIAL forward tcp:38474 tcp:38473
idfr --url http://127.0.0.1:38474 devices
```

## Library boundary

Enable `idf-remote` with `default-features = false, features = ["android-usb"]`.
The default `desktop` feature preserves the existing CLI and serial backend.

1. Construct `AndroidUsbBackend::with_cache_dir` with an app-private cache path.
   Open with `UsbManager`, duplicate its fd into `OwnedFd`, then call
   `AndroidUsbBackend::attach` off the UI thread.
2. Start `Service::start_many_in` with descriptors and an app-private cache path.
3. Use `submit_monitor`, `submit_serial_write`, `submit_application`, operation
   polling and `application_events`. Optionally serve `server::router(service)`.
4. On detach/close, stop workers and join them, release native resources, then
   close `UsbDeviceConnection`. The backend's `detach` invalidates an attachment;
   the old ID cannot be rebound to a new device.

IDs are attachment-scoped, not persistent physical identities. The backend uses
nusb's Android linux_usbfs detach-and-claim path to handle a kernel CDC driver;
other user-space owners remain protected. IO has short bounded timeouts, and
console writes submit buffered bytes without blocking for a drain. Bootloader
commands explicitly drain output and use protocol-specific timeouts. Console
success indicates bytes accepted by the driver, not a firmware acknowledgement.

The JNI sample keeps protocol decisions in Rust; its Kotlin UI is replaceable.
See [PLAN.md](PLAN.md) for validation and remaining work before PR review.

## Flashing through the phone

With loopback mode and ADB forwarding enabled, use the same desktop CLI:

```sh
idfr --url http://127.0.0.1:38474 probe
idfr --url http://127.0.0.1:38474 flash --build-dir ./build --monitor
```

The build directory is on the client computer; complete images are uploaded to
the phone and flashed locally over USB. Probe/flash interrupt application
communication. After firmware restarts, negotiate a new application session.

The Android dependency uses [Heap-Hop/espflash](https://github.com/Heap-Hop/espflash/tree/feat/android-transport)
at the commit pinned in Cargo.toml, based on upstream v4.5.0. Its optional
`custom-transport` feature replaces OS port handles with an owned byte-stream
and modem-control interface. USB permission and drivers remain in the embedding
app/backend. No global crates.io patch is applied.
