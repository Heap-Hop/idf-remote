# Connection setup and diagnosis

## Install and connect

For desktop hosts, install on both the development machine and USB host using
current stable Rust, if `idfr` is not already available:

```sh
cargo install idf-remote --locked
```

On the USB host, start the daemon if one is not already managing the board:

```sh
idfr serve
```

It binds `127.0.0.1:38473` and discovers supported USB serial devices dynamically.
To restrict discovery to specified current addresses, use
`idfr --port SERIAL_PORT serve`; `--port` is repeatable for `serve`.
Keep a newly started daemon in a managed session and record which process this
task owns. Do not leave duplicate servers competing for a port.

On the development machine, use an SSH tunnel to that loopback listener:

```sh
ssh -N -L 38473:127.0.0.1:38473 USER@USB_HOST
```

Leave the tunnel running and use a separate session for client commands:

```sh
idfr devices --json
```

If the local tunnel port is occupied, select another local port and pass its
URL explicitly to every client command. Do not kill an existing process to
free the port without authorization.

## Direct network access

Non-loopback listeners require a token file. When this deployment is requested,
use a securely provisioned token file on each side:

```sh
# USB host
idfr --token-file TOKEN_FILE serve --bind 0.0.0.0:38473

# Development machine
idfr --url http://USB_HOST:38473 --token-file TOKEN_FILE devices --json
```

The protocol is plain HTTP; the token does not encrypt it. Prefer the SSH
tunnel or an existing trusted encrypted network. Do not print token contents
or put them in command examples, logs, or commits.

## Diagnose in order

1. Verify the selected URL, tunnel endpoint, and daemon process on the USB host.
   Connection refused or a network timeout is not evidence of a faulty board.
2. For authentication failures, check that the correct token file is supplied
   and readable; do not disable authentication as a workaround.
3. Run `devices --json`. An empty list can mean missing USB attachment, USB
   permission, an unsupported transport, or a `serve --port` restriction.
   Inspect these on the USB host, not the build machine.
4. For unavailable/reconnecting devices, refresh descriptors and compare the
   expected USB identity. Reconnection can change the serial address.
5. For a busy device, inspect any known active operation and existing clients.
   Wait for legitimate work to complete; do not reset or stop another owner's
   service just to clear contention.

If active probing or a physical reset becomes necessary, explain why passive
checks were insufficient and ensure that interruption is authorized.

## Android host (experimental)

The Android app is an alternative USB host; it embeds the service and needs
Android USB permission. Its current scope is ARM64 Android 8+ with USB Host
and Espressif native USB Serial/JTAG. Keep the app visible during use; do not
assume background service support.

For LAN access, use the URL and token provided by the app's **HTTP / Display**
settings and pass them to `idfr` on the development machine. With LAN disabled,
an already configured wireless ADB connection can forward the loopback service:

```sh
adb -s PHONE_SERIAL forward tcp:38474 tcp:38473
idfr --url http://127.0.0.1:38474 devices --json
```

USB detach currently stops HTTP until reconnection. Rediscover the device after
reattachment; attachment IDs are not persistent physical identities.
For app installation and USB permissions, consult the
[Android guide](https://github.com/Heap-Hop/idf-remote/blob/master/android/README.md).
