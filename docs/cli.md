# CLI and live fleet view

`ballast top` shows memory pressure and its inputs, the guardian's explanation, paused and held work, and the owner/agent/workload tree.
It polls once per second (or the daemon interval if slower); keyboard input remains responsive while a daemon request is pending.
Press `q`, Escape or Ctrl-C to exit; arrows or `j`/`k` scroll; Page Up/Down and Home/End navigate longer fleets.
Use `ballast resume <workload-id>|--all` or `ballast stop <agent-id>|<workload-id>` from another terminal.
The view is read-only.
External SIGTERM, SIGINT and SIGHUP exit through terminal restoration.

Colors inherit the terminal's foreground and background, with semantic accents: green Normal, ochre Elevated, red Critical, blue frozen and magenta held.
`NO_COLOR` disables accents; state labels remain explicit.
Unchanged views skip drawing, and Ratatui sends changed cells only.
At 110 columns the fleet uses separate state, class, CPU, memory, age, tree label and ID columns.
The final ID column follows the longest fleet identifier, capped at 40 cells; labels use the remaining width.
From 64 columns it combines workload labels and IDs; smaller terminals stack metrics.
Agent and workload branches retain sibling continuations.
Layouts separate sections with blank lines when the entire view fits; short terminals drop those gaps first.
Text clips at grapheme boundaries; full IDs remain available through `ps`.
Memory and swap meters show used/total; a zero swap total displays `no swap`.

CPU is a delta between valid samples, where one busy core is 100% and multiple cores may exceed 100%.
Agent CPU includes all attributed members; workload CPU includes only that workload's members.
`?` means an unavailable measurement or a warming CPU baseline; `~` marks partial CPU or memory totals.
CPU sums available member deltas and shows `?` only when none is available.
The baseline resets on discarded samples, boot or identity changes, decreasing counters and gaps above five seconds.
Memory is physical footprint on macOS and RSS on Linux.
Compact table units M and G mean MiB and GiB.
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
| `processes` | Process records: `identity`, `ppid`, `pgid`, `uid`, `stopped`, nullable `name`, nullable `exe`, nullable `argv`, nullable `metrics` |
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

Pressure contains integer `page_size`, nullable byte counts `total_memory_bytes`, `used_memory_bytes`, `swap_used_bytes`, `swap_total_bytes`, nullable page counters `pageouts`, `swapins`, `swapouts`, nullable integer `kernel_pressure_level`, and nullable numbers `psi_some_avg10`, `psi_full_avg10`.
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
Older version-1 daemons may omit `held`, `guardian` and `pressure.swap_total_bytes`; interpret them as empty, unavailable and unknown respectively.
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
It captures 80-, 120- and 160-column light/dark panes under the ignored `spikes/out/` directory, then verifies admission and resume after pressure returns to Normal.
The fixture accepts `exit` in its pressure file, thaws on normal exit, and also exits after 150 seconds.

## Effectiveness report

`ballast report [--since 1d|7d|30d|90d] [--json]` reports recorded facts over local calendar days, including today.
The default is `7d`, today plus the six previous local days.
It reads through the running daemon, or directly from `state/stats.json` when disconnected.
Missing or corrupt data produces an empty report; corruption also prints a diagnostic on stderr.
The read-only command does not recover, signal, or modify state.

The daemon keeps the latest 90 local days independently of rotated decision logs.
Decision evidence and sampling intervals update cumulative aggregates on the tick thread.
A worker atomically replaces a private `stats.json` and syncs the file and containing directory.
A single-slot mailbox retains the latest cumulative aggregate; decisions trigger a write immediately, while sample-only changes persist at most every 30 seconds.
Clean shutdown flushes pending changes and joins the worker.
The top summary updates with each sample; reports read persisted data and sample-only totals can lag by 30 seconds.
A prolonged storage stall replaces the pending aggregate with its newer cumulative value; producers never wait for disk I/O and memory does not grow with a backlog.
An abrupt exit can lose not-yet-persisted updates.
Only aggregates are persisted, with no process identities, session IDs, commands, paths, arguments or environment contents.

Enforce outcomes and observe proposals are separate.
Observe freezes, holds and blocks say **would have**; simulated freeze durations are not actual paused time, and observe holds have no actual wait samples.
These are intervention counts and observed measurements, not estimates of work prevented or resources saved.
The `top` summary shows today's counters for its current mode, omitting trailing fields as width shrinks.

### Report JSON schema version 1

The CLI writes the report object directly, without the IPC envelope.
The IPC request is `{"version":1,"method":"report","since_days":7}` and its reply is `{"version":1,"type":"report","report":{...}}`.
Only 1, 7, 30 and 90 are accepted.
Additive fields deserialize with defaults; consumers should ignore unknown fields.

| Report field | Meaning |
| --- | --- |
| `schema_version` | Integer `1` |
| `updated_at_ms` | Latest aggregated Unix millisecond timestamp, or null for empty/older data |
| `since_days` | Requested local-day window |
| `from_day`, `through_day` | Inclusive local `YYYY-MM-DD` bounds |
| `days` | Map of recorded local dates to daily totals; `{}` means no records in this window |
| `totals` | Combined daily totals for this window |
| `hold_waits` | `enforce` and `observe` objects: `completed`, nullable `median_seconds`, nullable `worst_seconds` |

Daily and combined totals each have `enforce` and `observe` objects with these fields:

| Field | Meaning |
| --- | --- |
| `observed_ms`, `elevated_ms`, `critical_ms` | Valid sampled pressure time in milliseconds; unknown intervals, backward clocks and gaps over five seconds are excluded |
| `freezes_by_agent_kind` | Map from agent kind to freeze aggregates below |
| `holds` | Heavy command holds, or observe proposals |
| `hold_wait_seconds` | Sparse frequency map with integer-second string keys `0` through `300`; completed and cancelled waits are rounded down, capped at 300 |
| `timed_out_holds`, `cancelled_holds` | Completed holds released at the five-minute cap, and disconnected holds |
| `reclaimed_processes` | Confirmed exited processes after successful cleanup signals for ended agents |
| `reclaimed_memory_bytes`, `reclaimed_memory_unknown` | Memory in use when reclaimed (last observed at cleanup signalling), and count lacking a memory sample |
| `services_left_running` | Newly reported surviving service workloads, independent of notification delivery |
| `kills_blocked` | Cross-agent kill denials, or observe proposals |
| `forced_resumes` | Resumes at the ten-minute freeze cap, including simulated observe resumes |

For an even number of waits, the two middle buckets are averaged and rounded down to whole seconds; the worst wait is the largest occupied bucket.
A hold is counted on admission to the queue, while its wait is counted on completion, possibly on another local day.
Pending waits lost on daemon restart have no invented completion or wait measurement.
Reclaimed memory is the sum of per-process measurements, not a claim about memory returned to the OS; shared pages may overlap.

Each freeze aggregate has `count`, `total_ms`, `longest_ms`, `peak_memory_bytes`, `incomplete_memory_samples`, and `pressure_after_30s`.
Time accumulates across observed intervals and splits at local midnight, including daylight-saving boundaries.
`longest_ms` is the longest observed duration of one freeze touching that day; peak memory is the largest sampled concurrent frozen footprint for that agent kind.
Unknown memory samples make the footprint a lower bound.
`pressure_after_30s` maps pairs such as `critical->normal` to counts, assigned to the freeze's start day.
The after level is the first valid sample at or after 30 seconds, so it can be later if samples are unavailable.
A pending comparison is stored as `critical->unknown`; restart leaves it unknown.
These comparisons show correlation, not causation.

The on-disk object is `{schema_version, updated_at_ms, days}` with the same daily aggregates.
Snapshot adds defaulted `today: {day, enforce, observe}`; each mode contains `freezes`, `holds`, nullable `median_wait_seconds`, `reclaimed_memory_bytes`, `reclaimed_processes`, `services_left_running`, `forced_resumes`, and `kills_blocked`.
