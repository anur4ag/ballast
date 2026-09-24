# Architecture

Ballast is a per-user Rust daemon and CLI connected by a local Unix socket.
The daemon observes process identity, ancestry and pressure, attributes work to agents, and applies admission, freezing and cleanup decisions.
The terminal view displays the daemon's snapshot without making policy decisions.

Process identities combine a PID, an opaque start-time token and a boot identity for persisted state.
Signals independently revalidate identity.
Linux uses pidfds where available; macOS revalidates immediately before signalling, retaining the operating system's small PID reuse race.
Unknown readings stay unknown and uncertain ownership is left untouched.

New processes are fully read; watched and attributed processes refresh each tick, while unrelated processes refresh on a staggered five-second cadence.
Memory is physical footprint on macOS and RSS on Linux.
First samples and samples after long gaps invalidate rate baselines.
Observation failures retain the prior process table and expose the failure.

The guardian holds new heavy commands under elevated pressure and can freeze eligible batch workloads under critical pressure.
A journal records frozen identities for recovery after a crash, restart or upgrade.
Cleanup waits 30 seconds after confirmed agent exit, removes batch leftovers, and reports surviving services.
Explicit stops resume frozen work, send TERM, then KILL after five seconds if the same processes survive.
Observe mode records decisions without sending signals.

The daemon holds a lifetime lock on the Ballast home directory and repairs its socket by inode identity.
Deleting and recreating that entire directory while the daemon runs is unsupported.
Executable replacement is checked after complete ticks; the user service manager starts the upgraded executable and recovery runs before normal observation resumes.

## Configuration

`~/.ballast/config.toml` is optional:

```toml
mode = "enforce" # "observe" records decisions without acting
notifications = true
cleanup_grace_seconds = 30
log_max_bytes = 5242880
log_rotations = 3
```

Recovery sweeps registered agent markers by default.
`recovery_sweep_markers = ["MY_AGENT_KEY"]` limits the sweep to selected registered keys; `[]` disables the marker sweep.
Journaled processes are recovered regardless of this setting.

Operational logs, decision logs and local effectiveness reports live under the Ballast home.
The protocol is newline-delimited JSON, version 1, over `run/ballastd.sock`.
Requests are capped at 64 KiB with up to 64 simultaneous clients.
See the [CLI and JSON reference](cli.md) for snapshot and report schemas.
