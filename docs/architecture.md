# Architecture

Ballast is a per-user Rust daemon and CLI connected by a local Unix socket.
The daemon observes process identity, ancestry and pressure, attributes work to agents, and applies admission, freezing, macOS throttling and cleanup decisions.
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
Observe mode records decisions without freezing or throttling processes.
Startup recovery still restores previously enforced journal entries.

The daemon holds a lifetime lock on the Ballast home directory and repairs its socket by inode identity.
Deleting and recreating that entire directory while the daemon runs is unsupported.
Executable replacement is checked after complete ticks; the user service manager starts the upgraded executable and recovery runs before normal observation resumes.

## Configuration

`~/.ballast/config.toml` is optional:

```toml
mode = "enforce" # "observe" records decisions without acting
notifications = true
throttle = true # macOS only; false restores throttled work
cleanup_grace_seconds = 30
log_max_bytes = 5242880
log_rotations = 3

[throttle_pressure]
cpu_busy_fraction = 0.9
cpu_load_per_core = 1.0
agent_resource_share = 0.3
```

Frozen-process recovery sweeps registered agent markers by default.
`recovery_sweep_markers = ["MY_AGENT_KEY"]` limits the sweep to selected registered keys; `[]` disables the marker sweep.
Journaled processes are recovered regardless of this setting.

Operational logs, decision logs and local effectiveness reports live under the Ballast home.
The protocol is newline-delimited JSON, version 1, over `run/ballastd.sock`.
Requests are capped at 64 KiB with up to 64 simultaneous clients.
See the [CLI and JSON reference](cli.md) for snapshot and report schemas.

## macOS throttling

CPU has Normal/Elevated levels with two-sample entry and ten-second descent.
Entry requires a two-second host busy fraction above 0.9, one-minute load per core above 1.0, and eligible Batch workloads holding at least 30% of host CPU activity.
Once throttled, the journaled workloads' own CPU share keeps Elevated active while it remains at least 30%, even if backgrounding reduces host busy time.
When both the host entry condition and that demand condition stop holding, ten seconds of quiet releases the throttle.
Unrelated unthrottled workloads cannot retain the throttle, and new workloads require the host entry condition.
Independent I/O activation was dropped after the bounded measurement failed to establish foreground disk harm correlated with service-time counters.
DARWIN_BG still lowers both CPU and disk priority when the CPU trigger acts.
New live members are journaled and included on subsequent ticks.
Missing inputs, disabled throttling, failed operations, or loss of eligibility restore priority.
Memory admission and freezing run independently and take precedence in the tick.

`state/throttled.json` records all newly selected identities in one durable write before applying external DARWIN_BG.
The policy lowers CPU and disk I/O priority without stopping work and sends no notifications.
The external background flag is read through `proc_pidinfo`; `getpriority` cannot reliably read another process's state.
Pre-existing externally backgrounded descendants are recorded as preserved, never claimed as Ballast-owned policy.
Startup, `resume --all`, and uninstall share restoration of journaled identities and provable live descendants.
Manual resume also prevents rethrottling that workload for five minutes.

A child forked between journal updates can inherit background policy before being recorded.
If the parent exits before that child is sampled and journaled, ancestry can be lost even while the daemon is healthy.
Current attribution assigns a first-seen orphan a detached workload instead of reconnecting it to the original shell workload.
Such an orphan may remain at lower priority until it exits.
No marker sweep clears background policy that Ballast cannot prove it owns.
Failed restoration retains journal entries and retries; unreadable or corrupt throttle journals are reported rather than guessed.

Per-process two-second windows miss short-lived CPU workers, so lint-shaped churn remains unprotected in v0.1.
A future candidate is child CPU accounting from `proc_pid_rusage` on long-lived parents, subtracting previously observed exited-child time to prevent double counting.
The conservative busy gate also misses contention with substantial overall idle CPU, such as high load concentrated on performance cores or blocked on I/O.
