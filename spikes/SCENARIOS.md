# Bounded scenario measurements

Build once, then run all eight scenarios, including their baseline/enforce pairs with tiny loads:

```sh
cargo build --release --bin ballast --example scenario_native
python3 spikes/run_scenarios.py --output /tmp/ballast-wiring
```

Use a fresh output directory.
`--scenario 1` selects one pair.
`--mode baseline` or `--mode enforced` selects one half of that pair, including an explicitly authorized continuation after inspecting an emergency-aborted run.
Tiny mode uses 32 MiB total allocation, at most two CPU workers, 16 MiB of writes and three seconds of active work per run.
It verifies wiring and cleanup, not pressure protection.

Real Linux pressure runs are restricted to the disposable `lima-ballast-platform` guest:

```sh
python3 spikes/run_scenarios.py --pressure --output /tmp/ballast-pressure --stop-file /tmp/ballast-emergency-stop
```

The intended VM has four CPUs, 4 GiB RAM and 512 MiB swap.
Do not run pressure loads on a shared Linux host.
macOS pressure requires the user's prior approval and the additional `--mac-approved` flag.
The initial approved Mac measurements used `--memory-mib 2048`.
`--memory-mib N` can lower the automatic memory cap for an approved run.

| Bound | Pressure mode |
| --- | --- |
| Total load allocation | 96% of physical memory, capped at 7168 MiB, plus interpreter/control-process overhead |
| CPU workers | Host core count, capped at ten |
| Active duration | 80 seconds for memory/origin/coupling; 60 seconds for CPU and disk |
| Disk | 128 MiB maximum file size, 8192 MiB maximum cumulative writes, paced across the run |
| Emergency stop | External stop file, swap growth over 384 MiB within a run, two successive sleep overshoots over 1000 ms, or observer/daemon exit |
| Independent deadline | A separate watchdog resumes and terminates registered identities ten seconds after the scenario deadline if normal cleanup has not finished |

The runner records exact native PID/start-time identities in a private temporary directory.
Normal cleanup first resumes stopped workers, lets agent roots reap their children, terminates remaining registered identities, verifies that they have disappeared and removes the temporary directory.
The watchdog repeats identity-checked cleanup if the runner dies.
Control processes and workload processes have separate registries so the macOS Guardian cannot select the probe or controller.
Signals are never addressed by process name.
Before each pressure run, the runner requires three quiet checks, two seconds apart, with a 60-second limit and non-growing swap; otherwise it skips remaining runs.
An emergency abort stops the sequence for inspection.

Each run uses a temporary home, a unique recovery marker key and `notifications = false`.
The macOS platform wrapper also refuses notification delivery and signals outside its owned identity set.
No real Claude or Codex home is read or modified.

## Host scope

Linux uses the unmodified `ballast daemon` executable and real `ballast hook claude` admission calls from the stand-in agent.
The baseline makes the same hook calls without a daemon, exercising their fail-open path.
The guest must contain no unrelated agents.

macOS uses the unchanged Observer, Attributor and Guardian behind an owned-identity platform wrapper.
For scenarios 1 through 7, it reads real host pressure without daemon IPC admission, cleanup policy or hook state transitions.
Scenario 8 adds the production IPC Server, Admission and HookState behind the same owned platform; cleanup policy remains outside its scope.
Guardian ticks follow the production normal/fast cadence, while observation traces are sampled at 250 ms in both baseline and enforced runs.
The trace collector performs the same owned attribution work in each mode.
The macOS result must not be presented as full daemon end-to-end coverage.

The stand-in root is a copy of the Rust helper named `claude` carrying real Claude session markers.
Shell workloads are direct children of that root.
The MCP stand-in is a direct internal child whose worker stays in its group.
The escaped variant uses a private `.app` via `open -a` on macOS, and a detached, environment-scrubbed `setsid` launch in Linux.
The macOS `open` environment is also scrubbed so the app exercises the unmarked escape rather than marker recovery.

## Evidence

Each run directory contains `result.json`, foreground probe samples, a pressure/attribution trace, decision events, worker events, the exact plan, identities and cleanup verification.
The foreground probe touches an 8 MiB pre-faulted buffer and measures a 10 ms sleep's overshoot every 50 ms.
Results contain p50, p99 and maximum latency for both measurements.
`summary.md` compares the pairs.
A null first-action time means no freeze, hold or throttle was logged.
Baseline pressure level is null because no Guardian runs in that mode; raw native pressure inputs are still recorded.

Check attribution, observed pressure and worker outcomes before claiming a class is covered or broken.
A bounded run that never reaches a policy threshold is inconclusive about the policy response.
A watchdog or emergency stop is an aborted measurement, not a successful workload.
The load stand-ins measure these specific process shapes, not the performance of a real compiler, browser or model session.

## Passive threshold calibration

`scenario_native monitor REGISTRY STOP passive 600` uses Guardian Observe with an empty registry to record ten minutes without load or enforcement.
Set a fresh temporary `BALLAST_HOME` with notifications disabled and an empty recovery sweep list.
The trace includes the exact Guardian rates, policy tick timing and discarded-sample flags.

```sh
python3 spikes/analyze_pressure.py /path/to/trace.jsonl --output /tmp/ballast-calibration
```

The analysis first requires exact agreement between its hysteresis replay and every native Guardian tick.
It then compares rolling 5-second and 10-second counter rates and kernel-only input, retaining the same entry, descent and polling rules.
The output contains complete JSON timelines, a summary table and every upward native transition with its input, rate window and duration.
This is threshold evidence, not an enforcement effectiveness test.

## Hook latency under load

Scenario 8 runs ten concurrent release-build hooks for each of three commands under both memory and CPU load.
The deny commands name the other stand-in agent by PID and by a literal argv substring, exercising an on-demand native argv lookup.
These commands are sent as hook payloads and never executed.
The hold command is `cargo build --release`.
A small C library timestamps the first complete socket frame inside each real hook process, preserving peer credentials and ancestry.
The result includes time from the library constructor and time from process launch, plus hook exit time and output.
No-response calls remain in the quantile population as censored upper entries, shown as null when a percentile falls beyond received frames.

A hold requires an observed valid pressure gate and a running batch workload.
An admit is labelled `admitted, precondition not reached` and excluded from the fail-open denominator.
The Linux CPU case adds bounded memory allocation within the same memory cap to establish this gate.
The Mac CPU case adds no extra memory; an unestablished gate is inconclusive.
Baseline calls without a daemon establish the fail-open control and have no policy-failure denominator.
The runner cancels hooks only after observing Hold so a measurement does not wait for pressure recovery.

Tiny mode checks every path's wiring and requires all three result files per condition.
It does not guarantee a Hold because it does not create pressure.

## Natural recovery cycle

After the paired measurements, run scenario 1 once more in enforce mode:

```sh
python3 spikes/run_scenarios.py --pressure --recovery-cycle --output /tmp/ballast-recovery --stop-file /tmp/ballast-emergency-stop
```

This run keeps the same memory cap and staggered starts, but allows up to 720 seconds for the Guardian's own recovery cycle.
Each worker must allocate its fixed share and complete a final page-touch pass after the initial 90-second load phase.
Stopped workers can complete only after the Guardian resumes them.
The controller ends early when all four tasks have completed and the frozen queue is empty.
No harness resume occurs during this measurement; safety cleanup begins only after completion, an emergency or the deadline.
The JSON separates task completion events and the pre-cleanup pressure/freeze/resume timeline, including the Guardian's resume reasons.
A max-freeze resume or incomplete deadline is reported separately from normal FIFO recovery.
Tiny recovery mode uses the same completion path with 32 MiB and an eight-second deadline.

Scenario 2 fills large chunks sequentially from a pre-generated random 1 MiB page pattern.
This removes entropy generation from the timed allocation path while avoiding an all-zero payload.
The requested ramp is three seconds; the allocation-target event records the actual achieved rise time under pressure.

## Approved heavier Mac memory rerun

The separately approved `--mac-memory-rerun` profile requires `--pressure --mac-approved` and an explicit scenario 1, 2 or 8.
It raises the total allocation ceiling to 10240 MiB and the per-run swap-growth emergency stop to 2048 MiB.
Scenario 8 selects its memory condition only in this profile.
All other deadlines, owned-identity checks and emergency stops remain in force.
Stop Lima, check for concurrent builds, record ambient memory, then use `--memory-mib` to select ambient free memory plus about 3 GiB within that ceiling.
This profile is not permission to run pressure without the user's approval.


## Offline hook-phase reports

For a capture from a separately instrumented measurement build, `python3 spikes/analyze_hook_phases.py CAPTURE_DIRECTORY --output OUTPUT_DIRECTORY` joins the per-PID monotonic phase events with the hook audit records.
It reports per-phase p50/p99/max, internal deadline headroom and each path's worst internal-call timeline.
The analyzer generates no load and does not add instrumentation to product builds.
Hook and daemon phase intervals overlap, so the report rows must not be summed.
