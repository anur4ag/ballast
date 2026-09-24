# Ballast

Keep your machine responsive while coding agents work.
Early development, macOS and Linux only.

```sh
cargo run -- debug platform
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`debug platform` prints raw process and pressure data as JSON.
CPU time is cumulative nanoseconds, memory is bytes, and swap/pageout counters count pages.
Environment is read on demand and never included in debug output.

The platform API lives in `src/platform`.
`list_processes(&watched, &metric_targets)` enumerates PIDs every tick and fully reads new PIDs.
Watched identities and metric targets refresh every tick; other known processes refresh on a staggered five-second cadence.
Linux reuses known UID and argv between slow refreshes unless the executable changes; unknown argv is retried.
Unwatched state changes and PID reuse between enumerations can therefore lag five seconds plus one scan.
Policy code must watch agent roots, attributed processes and frozen identities before acting; signals independently revalidate identity.
The metric-target set selects which processes receive CPU/memory samples.
An empty set skips resource sampling; `process_metrics(identity)` remains available on demand.
The daemon will populate these sets from attribution in the next ticket; `debug platform` explicitly samples all discovered processes.
Unknown readings are `None`, including inaccessible processes and unreadable or truncated environments.
Linux memory is RSS; macOS memory is physical footprint.
Process identities combine PID and an opaque OS start-time token.
Persisted identities must also carry `boot_id()` and be discarded after a reboot.
Linux uses pidfds where available; macOS revalidates the identity immediately before `kill`, with the small residual PID reuse race inherent in that API.
Ports are queried separately from the fast scan.

Building on macOS requires the Xcode Command Line Tools for the small socket-info C shim.
Runtime dependencies are the system APIs; notifications use `osascript` on macOS and `notify-send` when available on Linux.
Successful invocation does not guarantee notification delivery by the desktop session.

Run `cargo run -- daemon` in the foreground as your normal user, then `cargo run -- status` or `cargo run -- status --json` in another terminal.
`BALLAST_HOME=/tmp/ballast-demo` overrides `~/.ballast` for both commands.
A second daemon using the same directory refuses to start.
The daemon holds a lifetime flock on `BALLAST_HOME` itself, so replacing `run/` cannot split ownership; socket repair checks its bound socket inode.
Deleting and recreating the whole base directory while a daemon runs is unsupported.
Other CLI commands remain placeholders.

The optional `config.toml` currently accepts these defaults:

```toml
mode = "enforce" # or "observe"; process actions arrive with the guardian
log_max_bytes = 5242880
log_rotations = 3 # 1 through 10 archived files per log
```

IPC uses newline-delimited JSON over `run/ballastd.sock` (protocol version 1).
For example, `{"version":1,"method":"status"}` returns `{"version":1,"type":"status","status":{...}}`.
`snapshot`, `ps`, and `top` return `type: "snapshot"` with a `snapshot` containing status, capabilities, boot identity, processes, process changes, and raw pressure inputs.
`resume` (optional `target`), `stop` (`target`), `gc`, and `hook` (`payload`) are routed to the tick loop and currently return an unimplemented error.
Errors use `{"version":1,"type":"error","message":"..."}`.
A connection supports multiple requests; malformed JSON and unsupported versions return errors, while incomplete or oversized lines close the connection.
Requests are capped at 64 KiB and the server admits up to 64 simultaneous clients.

The first sample and the first sample after a gap longer than five ticks are marked `sample_discarded`, with no pressure sample.
Consumers must reset rate baselines on this flag, including cumulative process CPU counters.
Observation errors retain the previous process table, expose `last_error`, and invalidate the sample.
The daemon writes rotated operational and decision logs under `log/`; later tickets provide attribution, pressure policy, and decisions.
