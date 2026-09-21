# Agent guidance

## Working agreements

- Inspect the branch and working tree before editing; preserve unrelated work.
- Keep changes within the requested scope. Avoid speculative abstractions and
  unrelated cleanup.
- Commit, push, and create pull requests only with user authorization. Existing
  authorization in the current conversation applies; do not repeatedly ask.
- Keep public documentation concise and in English. Use placeholders for device
  addresses, USB serials, hosts, credentials, and local paths.
- Do not commit firmware build output, flash dumps, tokens, or hardware logs.

## Project boundaries

- The project and Cargo package are `idf-remote`; the executable is `idfr` and
  the Rust library is `idf_remote`. Use `idfr` in command examples.
- Start with [README.md](README.md), [Architecture](docs/ARCHITECTURE.md), and
  [Testing](TESTING.md). Application protocol changes also require reading
  [Application protocol](docs/APPLICATION.md) and [Firmware](firmware/README.md).
- Each device worker owns its serial connection. HTTP handlers must not open
  ports or perform blocking hardware work. Preserve the backend trait boundary.
- A device ID is distinct from its current serial address. Preserve identity
  checks, bounded queues, and operation idempotency; never silently replay a
  command whose outcome is unknown.
- The daemon must not require the client's filesystem or ESP-IDF installation.
  Keep ordinary raw monitoring usable without the optional firmware component.
- Retain loopback defaults and authentication for non-loopback listeners. Check
  new dependency licenses for compatibility with this Apache-2.0 project.

## Verification

Use current stable Rust. Run checks appropriate to the changes; CI runs:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

For firmware codec changes, also run `python3 tests/test_mux_codec.py`. Follow
the firmware documentation for ESP-IDF builds. Report software checks and actual
hardware validation separately, including anything not run.

## Hardware and local services

- Hardware tests are opt-in. Use an authorized test board and verify its current
  address and expected identity; never assume an old path still names that board.
- Probe and reset can interrupt running firmware. Flash and erase can destroy
  data. Scope these operations to the authorized device and task.
- Do not open a port concurrently with another daemon or monitor. Do not stop
  user-managed services or remove test environments without authorization.
- Prefer fake transports or pseudo-terminals for host regressions. See
  `TESTING.md` for guarded physical tests; keep reports under `.artifacts/`.

## Commits and AI attribution

Write English Conventional Commit messages with an imperative summary, using
`type(scope): summary` where a scope is useful.

For substantial AI assistance, include an `Assisted-by` trailer identifying the
agent. Add a model identifier only when known explicitly; never guess it.

## Pull requests

Use `.github/PULL_REQUEST_TEMPLATE.md` if present. Otherwise, explain the concrete
problem, resulting behavior, relevant verification, and remaining limitations.
Do not present simulated tests as hardware evidence or planned features as
implemented support.
