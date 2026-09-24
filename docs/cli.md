# CLI and live fleet view

`ballast top` shows memory pressure and its inputs, the guardian's explanation, paused and held work, and the owner/agent/workload tree.
It polls once per second (or the daemon interval if slower); keyboard input remains responsive while a daemon request is pending.
Press `q`, Escape or Ctrl-C to exit; arrows or `j`/`k` scroll; Page Up/Down and Home/End navigate longer fleets.
Use `ballast resume <workload-id>|--all` or `ballast stop <agent-id>|<workload-id>` from another terminal.
The view is read-only.

Colors inherit the terminal's foreground and background, with bold headings and ANSI yellow for paused/waiting work and warnings.
Unchanged views skip drawing, and Ratatui sends changed cells only.
Narrow terminals wrap details and switch workload metrics to stacked rows below 64 columns.
Full IDs remain available by scrolling and through `ps`.

CPU is a delta between valid samples, where one busy core is 100% and multiple cores may exceed 100%.
Agent CPU includes all attributed members; workload CPU includes only that workload's members.
`?` means an unavailable measurement or a warming CPU baseline; `~` marks partial memory totals.
The baseline resets on discarded samples, identity changes, decreasing counters and gaps above five seconds.
Memory is physical footprint on macOS and RSS on Linux.
The guardian supplies pressure rates and explanations; the view never recomputes policy.
The level can lag the raw kernel/PSI signal because guardian hysteresis is intentional.

Disconnected or older-than-three-second samples are visibly marked.
The last fleet remains visible during reconnection.
Frozen-without-daemon warnings go to stderr, including for help/version requests, and name `ballast resume --all`.
Hook invocations remain silent to preserve fail-open operation.

## JSON contract, version 1

`ballast status --json` writes one JSON object and a newline to stdout:

```json
{"version":1,"type":"status","status":{}}
```

The `status` object always contains the fields below.

| Field | Type and meaning |
| --- | --- |
| `daemon_version`, `pid` | String version and integer daemon PID |
| `mode` | `enforce` or `observe` |
| `tick`, `sampled_at_ms`, `tick_interval_ms` | Integer tick, Unix sample milliseconds, polling interval milliseconds |
| `tick_cpu_ns`, `tick_wall_ns` | Integer CPU and wall time for the most recent daemon tick |
| `sample_discarded` | Boolean; invalidate CPU/rate baselines when true |
| `process_count` | Integer observed process count |
| `pressure_level` | `normal`, `elevated` or `critical`; may retain a previous level when input is unavailable |
| `batch_running` | Boolean; guardian's current running-batch signal |
| `cleanup_pending` | Array of opaque workload IDs |
| `last_error` | String or null |

`ballast ps --json` writes the IPC snapshot envelope:

```json
{"version":1,"type":"snapshot","snapshot":{}}
```

| Snapshot field | Type and meaning |
| --- | --- |
| `status` | Status object described above |
| `boot_id` | Opaque OS boot string; pair with process identities before comparing across snapshots |
| `capabilities` | Boolean keys: `environment`, `listening_ports`, `memory_footprint`, `memory_psi`, `kernel_pressure`, `notifications`, `atomic_signals` |
| `processes` | Process records: `identity`, `ppid`, `pgid`, `uid`, `stopped`, nullable `exe`, nullable `argv`, nullable `metrics` |
| `changes` | `started`, `exited`, `exec_changed` arrays of process identities |
| `pressure` | Raw pressure inputs or null |
| `attribution` | `owners`, `agents`, `workloads`, `processes` arrays described below |
| `frozen` | Records containing `workload_id`, `root`, `processes`, `frozen_at_ms`; a guardian freeze is for memory pressure |
| `held` | Admission-order records containing string `agent`, `session_id`, `label`, `reason`, and integer Unix `since_ms` |
| `guardian` | Current decision explanation or null: string `kind`, `message`, Unix `sampled_at_ms`, nullable `agent_memory_share`, `pageout_mib_per_sec`, `swapout_mib_per_sec` |

A process identity is `{ "pid": integer, "start_time": integer }`.
The start token is opaque and OS-specific.
Metrics contain integer `memory_bytes` and cumulative `cpu_time_ns`, never a percentage.
Environment contents are never serialized.
`argv`, workload labels and held-command labels can contain command text; treat output as private and terminal text as untrusted.

Pressure contains integer `page_size`, nullable byte counts `total_memory_bytes`, `used_memory_bytes`, `swap_used_bytes`, nullable page counters `pageouts`, `swapins`, `swapouts`, nullable integer `kernel_pressure_level`, and nullable numbers `psi_some_avg10`, `psi_full_avg10`.
Kernel values 1/2/4 mean normal/warn/critical; PSI averages are percentages.
Guardian rates are MiB/s; agent memory share is a ratio, not a percentage, and null when the decision did not evaluate it.
Decision kinds currently include `monitoring`, `normal`, `elevated`, `unknown_pressure`, `cooldown`, `non_agent_pressure`, `no_eligible_workload`, `last_batch_not_fastest`, `froze`, `freeze_failed`, and `resumed`.
Display the supplied message and tolerate new kinds.

| Attribution array | Record fields |
| --- | --- |
| `owners` | String `id`, nullable string `name` |
| `agents` | String `id`, `kind`; nullable strings `owner_id`, `session_id`, `cwd`; nullable identity `root`; `state` (`unknown`, `thinking`, `idle`, `ended`); nullable Unix `ended_at_ms`; `memory` |
| `workloads` | String `id`, `agent_id`, `label`; identity `root`; `class` (`batch`, `service`); Unix `first_seen_ms`; nullable integer `detached_pgid`; `memory` |
| `processes` | `identity`; nullable strings `owner_id`, `agent_id`, `workload_id`; `role` (`unattributed`, `agent_root`, `agent_internal`, `workload`); boolean `environment_known`; nullable integer-array `listening_ports`; nullable Unix `ports_sampled_at_ms` |

Memory summaries contain integer `bytes`, boolean `complete`, and nullable signed `growth_30s_bytes`.
All times and counters are integers unless explicitly documented as ratios/rates.
Null means unknown, not zero.
IDs are opaque; consumers must not parse their internal punctuation or depend on array order, except for admission ordering in `held`.
New fields can be added in version 1; ignore unknown fields.
Older version-1 daemons may omit `held` and `guardian`; interpret them as empty and unavailable respectively.
Existing field removal or incompatible type/meaning changes require a new protocol version.
Connection/protocol errors exit nonzero, with diagnostics on stderr and no successful JSON object on stdout.
Plain `ps` keeps one escaped record per line and includes cumulative process `cpu_ns`, frozen duration, admission wait/reason, guardian message, and pending cleanup IDs.
Use JSON for parsing rather than splitting plain text on spaces.

## Reproduce the terminal check

```sh
cargo build --release --example top_fixture --bin ballast
python3 spikes/verify_top.py
```

This uses tmux, a temporary Ballast home, production IPC/admission/guardian code, synthetic pressure, and three test-owned sleep processes.
It captures 80- and 160-column light/dark panes under the ignored `spikes/out/t09` directory, then verifies admission and resume after pressure returns to Normal.
The fixture accepts `exit` in its pressure file, thaws on normal exit, and also exits after 150 seconds.
