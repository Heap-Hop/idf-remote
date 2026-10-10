# Flash, monitor, and recovery

Examples use the default loopback URL. Carry the chosen `--url` and
`--token-file` options into every remote client command. Replace all uppercase
placeholders with verified task values.

## Validate artifacts before flashing

Build in the user's ESP-IDF project on the development machine when requested.
The daemon needs image bytes, not the build toolchain or a shared directory.
Inspect an existing build without hardware access:

```sh
idfr plan --build-dir BUILD_DIR
```

This imports `flasher_args.json`, resolves local image paths, and validates the
plan and artifacts. It does not check the physical board's identity, capacity,
or security state. Empty manifest images are skipped with a warning; inspect
warnings instead of assuming every manifest entry will be written.

For a portable plan supplied by the user:

```sh
idfr plan --plan PLAN_FILE
```

Image paths in a portable plan are relative to the plan file. Do not combine
`--plan` with `--build-dir` or `--image`. Inspect offsets and chip type rather
than inventing a flash layout. Use `--image app` only when an application-only
update is intended and the board already has compatible supporting images.

## Flash and collect evidence

After selecting and verifying the authorized board, flash the validated source:

```sh
idfr --device DEVICE_ID flash --build-dir BUILD_DIR
```

The daemon verifies chip type, flash capacity, security state, and image ranges
before writing. Secure-boot and flash-encryption provisioning are unsupported;
do not bypass a validation failure.

When the task includes checking boot output, use a bounded capture. `--wait`
is a regular expression and implies monitoring for flash/reset:

```sh
idfr --device DEVICE_ID flash --build-dir BUILD_DIR --wait 'EXPECTED_BOOT_PATTERN' --timeout 30
```

Choose the expected pattern from the user's application or known fixture.
A write/verify success and a boot-marker match are separate evidence. An error
after writing starts may leave a partial image; do not automatically reset into
it or erase flash as a generic recovery step.

## Monitor and console input

Observe without forwarding stdin or requesting a reset:

```sh
idfr --device DEVICE_ID monitor --no-input --timeout 15
idfr --device DEVICE_ID monitor --no-input --wait 'EXPECTED_PATTERN' --timeout 30
```

`--timeout` is in seconds; `--wait` defaults to 30 seconds when no timeout is
given. Use `--raw-log OUTPUT_FILE` for exact serial bytes when needed; it
creates a new file and refuses to overwrite an existing one. Match the firmware
baud with `--monitor-baud` when it differs from the default 115200.

For a user-requested interactive session, `idfr --device DEVICE_ID monitor`
forwards keyboard input. `Ctrl-]` exits; `Ctrl-C` is sent to the device.
Do not use `Ctrl-C` as the planned way to close an interactive monitor.

Explicit console input is available without a reset:

```sh
idfr --device DEVICE_ID serial-write --text 'COMMAND_TEXT' --newline
```

Send only the intended bytes; omit `--newline` if no LF is wanted. Input is
limited to 64 KiB. Driver acceptance does not prove that firmware processed
the command; check the application's response when required.

Monitoring can reconnect after USB interruptions, but early boot output may
be lost before the operating system recreates the endpoint. Missing output
alone does not justify a reset or imply that the firmware failed to boot.

## Disruptive operations

- `probe` actively enters the ROM loader and resets the board. Use only when
  chip identification and the interruption are within the authorized task.
- `reset` interrupts the application. For an authorized reset with log evidence,
  use `idfr --device DEVICE_ID reset --wait 'EXPECTED_BOOT_PATTERN' --timeout 30`.
- `read-flash` also enters the ROM loader. Check its help, bound `--offset` and
  `--size`, and choose a new `--output` file. Treat dumps as potentially sensitive.
- `erase-flash` destroys the entire flash and requires the CLI's explicit
  confirmation value. Use it only for an explicitly requested full erase of
  the verified board, with a known recovery image when restoration is intended.
  Do not treat it as a fallback for a failed flash.

## Unknown outcomes and retry keys

Keep the operation ID shown in structured output or errors. Client interruption
can leave server-side work running. Query without a device selector:

```sh
idfr operation OPERATION_ID
```

Use bounded polling for a running operation. Investigate a failed operation's
reported stage before choosing recovery. If submission lost its response, a
known `--request-key` can recover the same operation only while the same daemon
retains it and the request content is identical. Do not generate a new key to
retry uncertain work. An unknown/expired operation or daemon restart requires
re-establishing state and deciding explicitly whether a new operation is safe.
