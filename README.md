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

Install the built binary from its permanent location with `ballast install`.
This starts a user service and adds Claude Code and Codex hooks while preserving existing hooks and making timestamped backups.
Open Codex and use `/hooks` to approve the Ballast hooks; repeat approval after moving the binary or changing the hook definition.
`ballast doctor` checks installation, hook trust, recovery state and platform access; `ballast doctor --notify` submits a test desktop notification.
`ballast uninstall` resumes frozen work and removes the service and Ballast hooks; `--purge` also removes Ballast's configuration, state and logs.
Rerun `ballast install` after upgrading or moving the binary.
For isolated installs, paths respect `HOME`, `BALLAST_HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_CONFIG_HOME` (Linux), `BALLAST_SERVICE_DIR` and `BALLAST_SERVICE_LABEL`.
Use an absolute path for each directory override and a unique service label for tests.
Linux requires a working systemd user session; headless systems without a notification service report that limitation in doctor.
Doctor reads user-level Codex trust; project or managed settings can still override runtime hook behavior.

Run `cargo run -- daemon` in the foreground as your normal user, then `cargo run -- status` or `cargo run -- status --json` in another terminal.
`BALLAST_HOME=/tmp/ballast-demo` overrides `~/.ballast` for both commands.
A second daemon using the same directory refuses to start.
The daemon holds a lifetime flock on `BALLAST_HOME` itself, so replacing `run/` cannot split ownership; socket repair checks its bound socket inode.
Deleting and recreating the whole base directory while a daemon runs is unsupported.
`ballast gc` immediately cleans ended agents and lists their surviving services and unattributed dev-tool orphans.
`ballast stop <agent-id>|<workload-id>` stops the selected workloads, including services, without stopping the agent or its internal processes.
Find IDs with `ballast ps`.
Accepted stops remain pending through uncertain membership; `ballast gc` and `ballast status` list the pending workload IDs.
Initial TERM waits for positive workload membership; explicit-stop escalation follows the exact identities that received TERM, with agent roots protected.
Both cleanup commands return after scheduling termination; the daemon resumes frozen work, sends SIGTERM, and sends SIGKILL to survivors after five seconds.
Automatic cleanup waits 30 seconds after confirmed agent exit, reclaims batch and agent-internal leftovers, and reports services without terminating them.
Observe mode records these decisions without signalling processes.

The optional `config.toml` currently accepts these defaults:

```toml
mode = "enforce" # or "observe"
notifications = true # false keeps notification decisions in the log without desktop delivery
cleanup_grace_seconds = 30
log_max_bytes = 5242880
log_rotations = 3 # 1 through 10 archived files per log
```

Recovery sweeps all built-in and custom marker keys by default.
Set `recovery_sweep_markers = ["MY_AGENT_KEY"]` to limit the stopped-process sweep to registered keys, or `[]` to disable it.
Journaled processes are recovered regardless of this setting.

IPC uses newline-delimited JSON over `run/ballastd.sock` (protocol version 1).
For example, `{"version":1,"method":"status"}` returns `{"version":1,"type":"status","status":{...}}`.
`snapshot` and `ps` return `type: "snapshot"` with status, capabilities, boot identity, processes, process changes, attribution, frozen/held work, guardian explanations, and raw pressure inputs.
`top` returns the same shape projected to attributed processes, without executable/argv data or process changes; machine-wide totals and pressure inputs remain intact.
`resume` (optional `target`), `stop` (`target`), `gc`, and `hook` (`payload`) are routed to the tick loop.
Cleanup requests return `type: "cleanup"` with a `report` containing `observe`, `scheduled` and `pending` target IDs, `services` (workload ID, PIDs, ports), and `orphans` (identity and executable basename).
`ballast hook claude|codex` bridges admission and lifecycle hooks to the daemon and fails open when unavailable.
Errors use `{"version":1,"type":"error","message":"..."}`.
A connection supports multiple requests; malformed JSON and unsupported versions return errors, while incomplete or oversized lines close the connection.
Requests are capped at 64 KiB and the server admits up to 64 simultaneous clients.

The first sample and the first sample after a gap longer than five ticks are marked `sample_discarded`, with no pressure sample.
Consumers must reset rate baselines on this flag, including cumulative process CPU counters.
Observation errors retain the previous process table, expose `last_error`, and invalidate the sample.
The daemon writes rotated operational and decision logs under `log/`; later tickets provide attribution, pressure policy, and decisions.

Run `ballast top` for the live fleet, or `ballast ps --json` for scriptable snapshots.
See [CLI controls and JSON schema](docs/cli.md).
