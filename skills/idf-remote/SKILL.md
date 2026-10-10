---
name: idf-remote
description: >-
  Use the idfr CLI to flash and monitor ESP devices through a remote USB host.
  Use for idf-remote setup, device selection, firmware flashing, bounded log
  capture, connection troubleshooting, and optional application gateway calls.
license: Apache-2.0
---

# idf-remote

Operate ESP devices using `idfr`, the CLI installed by the `idf-remote` Cargo
package. The development machine builds firmware and uploads complete images;
the USB host owns the board connection and runs the daemon. The USB host does
not need ESP-IDF or access to the development machine's filesystem.

This skill covers using the tool from an ESP project. For changes to the
idf-remote implementation, follow that repository's `AGENTS.md` and development
documentation. Ordinary Rust or ESP-IDF development alone does not need this skill.

## Establish the environment

1. Identify the development machine, USB host, daemon URL, and requested board.
   Reuse an existing daemon or tunnel when available.
2. Check `idfr --version` and `idfr --help`. Use `idfr COMMAND --help` to check
   options against the installed version; the CLI and gateway are evolving.
3. Read [connection setup](references/connection.md) when installation, remote
   access, authentication, Android hosting, or connectivity needs attention.
4. Discover current devices without opening or resetting them:

   ```sh
   idfr --url http://HOST:PORT devices --json
   ```

   Add `--token-file TOKEN_FILE` to client commands when the daemon requires it.
   Carry the selected URL and authentication options through subsequent commands.

## Select the intended device

Use the descriptor's identity, transport metadata, availability, and capabilities
to match the user's board. Prefer `--device DEVICE_ID` from the current listing;
`--port SERIAL_PORT` is an alternative locator, not an identity check. Do not
combine them. Multiple boards require an explicit selection. If the intended
board cannot be identified, ask for the missing identifying information before
acting on hardware.

Device IDs are not persistent pairing credentials. Refresh discovery after a
daemon restart or device reattachment and verify the match again. Never assume
an old serial address still names the same board.

`probe` enters the ROM bootloader and resets the device. Do not use it as a
passive discovery step. When active chip identification is needed, ensure the
interruption fits the user's authorized task and target.

## Choose the workflow

- **Flash or inspect firmware artifacts:** Read
  [flash and monitor](references/flash-and-monitor.md). Validate the client's
  build artifacts with `plan` before a requested flash; plan validation alone
  does not verify the connected hardware.
- **Observe logs:** Prefer a bounded, input-disabled capture:

  ```sh
  idfr --url http://HOST:PORT --device DEVICE_ID monitor --no-input --timeout 15
  ```

  This does not request a reset. Add `--wait 'EXPECTED_PATTERN'` when the task
  has a known log condition. Do not reset merely because early boot logs are
  absent. Use an interactive monitor only when interaction is wanted.
- **Send serial input, reset, read, or erase flash:** Read the relevant section
  of [flash and monitor](references/flash-and-monitor.md) and the subcommand's
  help. Match the action and target to the user's existing authorization.
- **Call application methods:** Read
  [application gateway](references/application-gateway.md). This requires
  compatible firmware and explicit negotiation. Raw flash/monitor workflows
  need no gateway firmware.

## Preserve operation boundaries

The daemon's device worker owns its serial connection. Do not open the same
port with another daemon, `idf.py monitor`, a serial terminal, or an embedded
host program. Do not stop user-managed services to resolve contention without
authorization.

Flash, reset, probe, flash reads, and erase can interrupt firmware. Flash and
erase can destroy data; console input and application methods can also change
device state. A request to inspect logs does not authorize these actions.
Reuse clear authorization already provided for a concrete action and board;
ask only when the required scope or target is missing.

If an operation's outcome is unknown, inspect its operation ID before deciding
what to do next:

```sh
idfr --url http://HOST:PORT operation OPERATION_ID
```

Do not pass `--device` or `--port` to `operation`, `devices`, or `plan`.
The CLI generates a request key unless `--request-key REQUEST_KEY` is supplied.
A same-key, same-content retry can recover a retained operation, but records
are bounded and in memory. A restart or eviction removes that protection.
Never silently resubmit an uncertain command with a new key or after losing
the operation record. In particular, application timeouts do not prove that
the method did not execute.

## Report evidence

Report the selected target, action, operation status, and relevant captured
output. Distinguish artifact validation, simulated checks, and actual hardware
results. A successful flash does not by itself prove that the application booted
or that a gateway session was established. State missing evidence explicitly.

Use placeholders in shared examples. Keep tokens, flash dumps, build outputs,
and hardware logs out of commits. In an idf-remote checkout, put local reports
under `.artifacts/`; elsewhere use the user's chosen output location.
