# Verification spikes: 2026-09-24

These are observations on this machine, not calibrated cross-machine defaults.
Raw output is retained locally under `spikes/out/`, which is gitignored.
No product files or global agent settings were edited, and no commits were made.

## Environment and reproduction

- macOS 27.0 (26A428), arm64, 16 GiB RAM, 10 logical CPUs, 16,384-byte VM pages.
- Claude Code 2.1.281 and Codex CLI 0.156.1 were invoked with tiny fixture prompts.
- Claude used `--settings <temporary file> --setting-sources '' --strict-mcp-config --no-session-persistence`.
- Codex used an isolated temporary `CODEX_HOME` containing a copied `auth.json` and fixture `hooks.json`, then removed that directory.
- Codex hook trust bypass was scoped to each test invocation, as authorized for these probes.
- Initial macOS swap was 2,719.06 MiB used, with normal kernel pressure and zero swapout rate during the 15-second measured baseline.
- The only VM during the Mac load experiments was this spike's idle `ballast-spike` Debian VM, allocated 1 GiB and 2 CPUs.
- The coordinator was asked to pause the other implementation agent's builds and VM starts during the Mac measurements.
- Small hook probes overlapped the memory test, so this was a realistic occupied workstation rather than an isolated benchmark.
- Baseline evidence is in `out/mac-baseline.log` and `out/mac-vms.log`.

```sh
python3 spikes/memory_probe.py
python3 spikes/cpu_probe.py
python3 spikes/run_hooks.py claude hold
python3 spikes/run_hooks.py claude stop
python3 spikes/run_hooks.py codex hold
python3 spikes/run_hooks.py codex timeout
python3 spikes/run_hooks.py codex crash
python3 spikes/run_hooks.py codex post
python3 spikes/run_hooks.py codex stop
BALLAST_LEGACY_EXEC=1 python3 spikes/run_hooks.py codex stop
python3 spikes/notification_probe.py
/usr/bin/script -q spikes/out/codex-tui.typescript python3 spikes/tui_probe.py
python3 spikes/check_evidence.py
```

The Mac memory run is capped at 180 seconds, starts four owned compiler process groups, stops one at critical pressure or 65 seconds, and cleans all of them by 110 seconds or after 45 continuous critical seconds.
An independent watchdog bounds the controller, and cleanup resumes stopped processes before terminating their owned groups.
The CPU control uses ten workers for 30 seconds, each with its own 35-second lifetime ceiling.
The scripts never select another application's processes for signaling.
The interactive Codex fixture has a 150-second deadline and removes its temporary home after its owned process group exits.
The headless CLI probes have a 180-second controller deadline and clean their own process groups and recorded fixture child.

## 1. Memory versus CPU

**Answer: the experiment supports memory pressure as a useful separate signal, but does not establish that actual UI freezes are exclusively memory-driven.**
Four optimized C++ builds produced warning pressure and much larger swap bursts than the CPU-only control.
No kernel-critical sample or verified unusable UI occurred, so there is no measured "unusable" threshold.
The latency measurement is `/usr/bin/true` process-launch latency, not an input-to-paint or human UI measurement.

The workload was four parallel `clang++ -O2 -c -std=c++17` compilations of 500,000 explicit template instantiations, with separate output objects.
A preliminary syntax-only run completed too quickly and is retained separately in `out/mac-memory-preflight.jsonl`; it is not used for threshold proposals.

| Phase | Samples | Kernel levels | Peak swapout MiB/s | Launch median / p95 / max ms |
| --- | ---: | --- | ---: | --- |
| Idle baseline | 15 | Normal | 0.00 | 6.66 / 7.94 / 9.04 |
| Four builds before stop | 47 | Normal, warning | 682.06 | 6.58 / 15.17 / 123.79 |
| One build stopped | 42 | Normal, warning | 405.71 | 5.44 / 10.90 / 20.76 |
| All builds cleaned up | 66 | Normal | 0.00 | 6.49 / 8.12 / 12.01 |
| Ten CPU workers | 30 | Normal | 54.76 | 6.65 / 18.08 / 27.81 |

Rates use actual elapsed sample time and `vm_stat` page size, not an assumed 4 KiB page or nominal one-second interval.
Peak rates are short bursts; the median swapout rate in every phase was zero.
Evidence: `out/mac-memory.jsonl`, `out/mac-cpu.jsonl`, and the executable calculations in `check_evidence.py`.

Linux was tested in a disposable Debian arm64 VM with kernel `6.1.0-31-cloud-arm64` and 1,004,436 KiB guest RAM.
`/proc/pressure/memory` initially reported `some avg10=0.00` and `full avg10=0.00`.
A systemd user service with `MemoryHigh=96M`, `MemoryMax=256M`, `MemorySwapMax=0`, and `RuntimeMaxSec=45` attempted a 160 MiB allocation.
Global PSI reached `some avg10=96.20` and `full avg10=95.69`, with cgroup `oom=0` and `oom_kill=0` throughout.
This verifies usable PSI signals under controlled reclaim throttling, not Linux desktop freeze thresholds.
Evidence: `out/linux-baseline.log`, `out/linux-probe.log`, and `linux_probe.sh`.

## 2. Freeze relief

**Answer: one stopped build was followed by temporary pressure relief within 2.13 seconds; it did not keep pressure normal while three builds continued.**
At 65.45 seconds, the test stopped compiler PID 41924 and its driver process group 41912.
The compiler had 1,150,192 KiB RSS and had grown 427,248 KiB over approximately 30 seconds, the largest RSS growth among the four compilers at that sample.
Kernel pressure changed from warning to normal at 67.58 seconds, then returned to warning 5.19 seconds later.
The stopped compiler remained stopped and its RSS later fell to 23,024 KiB as its pages were reclaimed; SIGSTOP does not itself free allocations.
All compiler groups were resumed and terminated at 110.05 seconds, after which all remaining samples were normal with zero swapouts.
This is a temporal association from one run; compiler phases and reclaim were already changing before the stop, so it is not a controlled proof of recovery causality.
The observation supports rechecking pressure and potentially stopping another workload, rather than assuming one stop solves a multi-build overload.

## 3. Claude Code hold semantics

**Answer: a 90-second PreToolUse hold did not consume the requested three-second Bash timeout.**
The hook ran for 90.006 seconds, then `sleep 2; printf BALLAST_COMMAND_DONE` completed successfully with Bash `timeout: 3000` and `interrupted: false`.
The successful tool-result timestamp was 2.825 seconds after the hook's end timestamp.
Stream output included `hook_started` with `hook_event: PreToolUse` and `hook_name: PreToolUse:Bash`, followed by `hook_response` when it finished.
The interactive Claude UI was not exercised because these tests used the specified `claude -p` isolation method; exact interactive wording remains unverified.
Evidence: `out/claude-hold-hook.jsonl`, `out/claude-hold.jsonl`, and `out/claude-hold-status.json`.

## 4. Agent supervision under SIGSTOP

**Claude: the three-second Bash timeout backgrounds a stopped command; ending the headless session kills that background task.**
Two trials emitted a background task ID after the timeout, then `task_updated` with `status: killed` and `task_notification` with `status: stopped` during headless shutdown, before controller cleanup.
This qualifies the research's previous blanket statement that freezing a Claude-launched job does not kill it.
The coordinator accepted the qualification: timeout itself backgrounds the command, while session-end cleanup is a separate agent decision.
Interactive Claude session-exit behavior was not checked.
Evidence: `out/claude-stop.jsonl` and `out/claude-stop-headless-exit.jsonl`.

**Codex default execution: a stopped command survived 15.06 seconds, resumed, and exited successfully.**
The fixture stopped at epoch 1790199044.437 and resumed at 1790199059.496, then returned exit code 0.
The current unified execution path yields a running session rather than imposing the requested three-second command deadline, so this proves freeze/resume compatibility for that path, not survival of a real hard timeout.
Disabling `unified_exec` alone under GPT-6-Astra still yielded and resumed successfully; it did not establish a hard-timeout path.
A further attempt disabled both `unified_exec` and `code_mode_host`; an initial GPT-5.4 request was rejected by the account, and a retry with locally listed GPT-5.6-Sol reported that code execution was unavailable because the code-mode host was disabled.
No fixture command ran in either attempt, so this installed build did not expose a usable legacy hard-timeout path through those settings.
Hard-timeout behavior remains unverified.
Evidence: `out/codex-stop.jsonl`, `out/codex-stop-status.json`, `out/codex-stop-legacy-astra.jsonl`, `out/codex-stop-legacy-unavailable.jsonl`, and `out/codex-stop-legacy.jsonl`.

## 5. Codex hooks

**Answer: hook timeout and crash failed open, a 90-second hold completed, and PostToolUse additionalContext reached the model.**
A `PreToolUse` hook configured with `timeout: 2` that slept for 20 seconds recorded only its start, after which the command completed with exit code 0.
A hook that killed itself with SIGKILL likewise allowed the command to complete with exit code 0.
The successful hold lasted 90.016 seconds and the command ran afterward; total headless invocation time was 105.37 seconds.
A PostToolUse hook injected the unique token `BALLAST_CONTEXT_7F19` through `hookSpecificOutput.additionalContext`, and the model included that token in its final reply even though it was absent from the prompt and command output.
An initial Codex hold trial ended the model turn while the execution cell was still running; it is labeled `codex-hold-incomplete` and excluded from the successful hold evidence.
The successful rerun explicitly asked the model to poll yielded work until completion.
A real Codex TUI showed `Working`, an elapsed-time indicator, `esc to interrupt`, and the configured status text `Ballast verification hold` while the hook was running.
A first TUI trial completed its 90.003-second hold and printed `BALLAST_TUI_DONE`; a second trial also completed its hold and captured the terminal text in `out/codex-tui.typescript` and `out/codex-tui.text`.
Both TUI fixtures were terminated by their controller after the successful turn when the 150-second interactive-session deadline elapsed.
The installed hook can therefore supply meaningful status text instead of relying on a generic hook-running indicator.
Evidence: `out/codex-timeout*`, `out/codex-crash*`, `out/codex-hold*`, and `out/codex-post*`.
The hook fixture uses the documented schema from [OpenAI's hooks guide](https://learn.chatgpt.com/docs/hooks).

## 6. Notifications

**macOS: the temporary GUI-domain LaunchAgent successfully executed `osascript display notification`; visible banner delivery was not verified.**
`launchctl bootstrap gui/501 <temporary plist>` returned 0, the job ran once with `last exit code = 0`, stderr was empty, and `launchctl bootout` returned 0.
A `com.apple.usernotificationcenter.matching` event channel appeared in the job record.
This proves service-context submission works on macOS 27.0, not that Focus settings or Notification Center permissions allow a visible banner.
Evidence: `out/notification.json` and `notification_probe.py`.

**Linux: desktop delivery is infeasible in the headless VM used here.**
The systemd user manager accepted and ran the pressure service, but neither `DISPLAY` nor `WAYLAND_DISPLAY` was set and no user notification bus was available.
After installing `libnotify-bin` and `dbus-user-session`, the notification service invocation targeting `/run/user/<uid>/bus` failed with `Failed to connect to bus: No such file or directory` before `notify-send` could execute.
This is an environment limitation, not evidence that `notify-send` fails in a logged-in Linux desktop session.
The VM was stopped and deleted after the probes; evidence is in `out/linux-cleanup.log`.

## Proposed initial thresholds

| Platform | Elevated E | Critical C | Justification and limit |
| --- | --- | --- | --- |
| macOS | Kernel warning, or pageout+swapout > 64 MiB/s | Kernel critical, or swapout > 256 MiB/s | E is above the 54.76 MiB/s CPU-control burst; C is a conservative protective proposal within observed build bursts, not a measured unusability boundary. |
| Linux | Memory PSI `some avg10 > 10` | Memory PSI `full avg10 > 5` or `some avg10 > 40` | Retain the plan's placeholders; the VM validated signal availability and response but cannot calibrate desktop tolerability. |

Retain two-sample escalation and ten-second de-escalation hysteresis.
The Mac thresholds are candidates for shadow-log tuning and require review, especially because rate bursts can occur without measured UI failure.
Do not present these results as validation that CPU can never impair responsiveness, that C is calibrated, or that notifications always reach the user.

## Remaining gaps

- Human-observed UI usability and an actual unusable-state rate boundary remain unmeasured.
- Critical-pressure recovery was not exercised; the bounded Mac test reached warning pressure only.
- Codex legacy hard-timeout behavior remains unverified because the default execution path yielded, while disabling the code-mode host made execution unavailable.
- Exact interactive Claude hold text and interactive session-exit cleanup remain unverified.
- Visible macOS notification delivery and delivery from a logged-in Linux desktop user service remain unverified.
