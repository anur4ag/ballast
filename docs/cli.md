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
| `guardian` | Current decision explanation or null: string `kind`, `message`, Unix `sampled_at_ms`, nullable `agent_memory_share`, `pageout_mib_per_sec`, `swapout_mib_per_sec`, `psi_some_percent`, `psi_full_percent` |

A process identity is `{ "pid": integer, "start_time": integer }`.
The start token is opaque and OS-specific.
Metrics contain integer `memory_bytes` and cumulative `cpu_time_ns`, never a percentage.
Environment contents are never serialized.
`argv`, workload labels and held-command labels can contain command text; treat output as private and terminal text as untrusted.

Pressure contains integer `page_size`, nullable byte counts `total_memory_bytes`, `used_memory_bytes`, `swap_used_bytes`, `swap_total_bytes`, nullable page counters `pageouts`, `swapins`, `swapouts`, nullable integer `kernel_pressure_level`, nullable numbers `psi_some_avg10`, `psi_full_avg10`, and nullable cumulative stall microsecond counters `psi_some_total_us`, `psi_full_total_us`.
Guardian PSI percentages use the sliding pressure window, falling back to avg10 before a valid counter delta exists.
Older daemons may omit the total counters and Guardian PSI percentages; interpret them as unknown.
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

Memory summaries contain integer `bytes`, boolean `complete`, nullable signed `growth_30s_bytes`, and nullable signed integer `growth_bytes_per_sec`.
Guardian uses `growth_bytes_per_sec`, truncated toward zero over 2 to 30 seconds of consecutive valid samples.
Older daemons may omit this field; interpret it as unknown.
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
| `throttled_workload_ms` | Sum of macOS throttled workload milliseconds; overlapping workloads add separately, with no extrapolation across gaps over five seconds |
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

## Install, repair and uninstall

`ballast install` previews the detected Claude Code and Codex configs and the per-user service before asking for consent.
Agent detection uses the config directory or an executable on `PATH`.
Enter or `y` applies; `n`, Escape or Ctrl-C cancels without writing.
`d` opens the complete file diffs; arrows, Page Up/Down and Home/End scroll, and Enter or Escape returns.
`c` opens an install checklist; arrows move, Space toggles and Enter confirms.
The service can be unchecked, but hooks cannot work without a running service.
The plan remains in terminal scrollback and inherits the terminal foreground and background, including with `NO_COLOR`.

`ballast uninstall` previews hook removal and service shutdown with the same consent and diff flow.
It resumes frozen work before removing the service and retains Ballast configuration, state and logs unless `--purge` is supplied.
Approved uninstall always runs the independent stopped-process recovery sweep, even without a service file or journal.
This can update runtime state and recovery logs even when no configuration changes or resumed work produce exit `2`.
Backups of changed agent configs are retained alongside those configs, including with `--purge`.
Unrelated hooks and settings remain intact.

For either command, `--dry-run` prints the plan and complete unified diffs without writing anything.
`--yes` applies without prompting and is appropriate only after the user has approved the changes.
A non-terminal invocation without either flag refuses to write and describes this two-step consent flow.
`--json` requires `--dry-run` or `--yes`, even in a terminal.
Re-running install repairs outdated hooks and services and skips unchanged files without creating new backups.
If files change after planning, installation refuses before writing; a failure during application reports the completed, failed and skipped steps.
Run the command outside the sandbox or approve access when it reports blocked agent directories or service-manager commands.
A partial install is never reported as success.

After install, inline doctor checks verify the service, daemon, hooks, trust and platform access.
Codex hook approval remains a user action through `/hooks`; Ballast never edits trust.
Hooks take effect in new agent sessions, so changed hooks require restarting the current session.
Desktop notifications are not sent by install or a normal doctor run; `ballast doctor --notify` explicitly requests a test notification.

### Installer JSON schema version 1

`ballast install --dry-run --json` and `ballast uninstall --dry-run --json` emit one plan object on stdout.
Paths in JSON are absolute; human plans abbreviate the home directory as `~`.
Treat plans as private because preserved agent settings appear in the diff.
Consumers must ignore unknown fields; all plan, item, file, result and check structs deserialize missing additive fields with serde defaults.

| Plan field | Meaning |
| --- | --- |
| `schema_version` | Integer `1` |
| `operation` | `install` or `uninstall` |
| `detected_agents` | Array of `claude` and/or `codex` |
| `items` | Ordered service, agent-hook and optional purge items |
| `requires_user_action` | User-facing instructions for Codex approval and new agent sessions |
| `current_session_needs_restart` | Whether planned hook changes require a new agent session |

Each item has string `id`, `purpose` and `detail`, booleans `selected` and `changed`, integer `existing_hooks_kept`, and a `files` array.
Each file has `path`, nullable `before` and `after` strings, a full-file unified `diff`, and nullable absolute `backup` path.
Null `before` creates a file and null `after` removes one.
Equal `before` and `after` means no write or backup.
The backup name uses local time, for example `settings.json.ballast-2026-09-25T10-02-41.bak`, with a numeric suffix only when that name already exists.
It is chosen during planning and is the name used during application of that plan.
Uninstall preserves backups from both this naming scheme and older versions.
A later command computes a fresh plan and fresh backup names from current files.
Service items also describe private runtime, state and log directories; purge items identify the entire Ballast data directory to remove.
Runtime files created by the daemon are not agent configuration diffs.

`ballast install --yes --json` and `ballast uninstall --yes --json` emit a result object:

| Result field | Meaning |
| --- | --- |
| `schema_version`, `operation` | Version `1` and requested operation |
| `status` | `success`, `no_change`, `refused`, `invalid` or `partial_failure` |
| `items` | Objects containing `id`, `status` (`applied`, `skipped`, `failed`) and explanatory `reason` |
| `doctor` | Check objects containing `name`, `status` and `detail` |
| `next_steps` | User-facing instructions; agents must report these honestly |
| `current_session_needs_restart` | Whether applied hook changes require a new agent session |

`ballast doctor --json` emits `{ "schema_version": 1, "operation": "doctor", "status": "healthy" | "attention_required", "checks": [...] }`.
Check statuses are `ok`, `failed`, `skipped` for deliberately unchecked components, or `action_required` for outstanding Codex trust.
Inline verification skips components excluded through the checklist, and selected-plan success reflects only the selected operations.
Unchecked service setup leaves a warning that installed hooks stay inactive until a daemon runs.
Standalone doctor still checks every detected component.
Install can succeed while Codex approval remains outstanding; doctor exits nonzero until the user completes it.
An invalid environment emits the same `invalid` result envelope as install and uninstall.

| Exit code | Meaning |
| --- | --- |
| `0` | Successful application, dry run, or healthy doctor |
| `1` | Doctor found a failed check or required user action |
| `2` | Apply found nothing to change; also used by clap for invalid CLI arguments |
| `3` | Consent refused, cancelled, or unavailable without a terminal |
| `4` | Invalid or unsafe environment/configuration; no writes by install/uninstall |
| `5` | Application or verification failed; some steps may already have applied |

Use the JSON `status` to distinguish `no_change` from CLI argument errors, which produce no result object.
See [Install with an agent](install-with-an-agent.md) for the full consent workflow.

### macOS throttle state

`guardian.throttle`, when present, contains `cpu_level`, `cpu_busy_fraction`, `agent_cpu_share`, `throttled_cpu_share`, and `workloads`.
The CPU level is `normal` or `elevated`; fractional measurements are null while unknown.
Each workload records `workload_id`, `root`, owned `processes`, and `preserved` identities whose external background policy predates Ballast.
`pressure.throttle` holds raw host CPU ticks and core/load counters.
Linux omits these optional fields.

`top` marks `THROTTLED` workloads, or `WOULD THROTTLE` in observe mode, and shows the CPU level.
`status` text shows the CPU level and throttle count.
`report` displays workload-seconds with separate enforce and observe accounting.
Decision events include `throttle`, `unthrottle`, and `throttle_pressure_transition` with measured evidence.
`resume --all` restores throttles as well as freezes, including when the daemon is unavailable.

`throttled_cpu_share` counts only journaled eligible identities and retains Elevated while their demand persists.
There is no independent I/O trigger; CPU-triggered background policy lowers both CPU and I/O priority.
