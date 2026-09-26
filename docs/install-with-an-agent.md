# Install Ballast with a coding agent

Follow this document when a user asks you to set up Ballast.
Ballast keeps the machine responsive while coding agents work, using a local per-user daemon and agent hooks.

## Rules

- Never pass `--yes` without the user's explicit approval of the installation plan.
- Never edit Claude Code or Codex configs by hand.
- Never approve Codex hooks on the user's behalf or write hook trust state.
- Ask the user to run or approve every `sudo` step.
- Report failed or skipped steps; a partially installed service is not a successful setup.
- Preserve existing environment overrides such as `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `BALLAST_HOME`.

## 1. Detect the environment

Run `uname -s` and `command -v ballast`, `command -v brew` and `command -v apt` as appropriate.
If Ballast is already available, continue to the plan step to check or repair the setup.
Use Homebrew on macOS and APT on Debian or Ubuntu.
On another system, explain that these are the supported package installation paths and stop before changing configuration.

## 2. Install the package

On macOS with Homebrew:

```sh
brew install anur4ag/tap/ballast
```

If `command -v brew` finds nothing, Homebrew is not installed.
Ask the user to install it from https://brew.sh themselves, because its installer asks for their password.
Once `brew` is on `PATH` in a new shell, run the command above.

On Debian or Ubuntu, show these privileged steps to the user and wait for their approval or ask them to run the commands themselves:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://anur4ag.github.io/ballast/key.gpg | sudo tee /etc/apt/keyrings/ballast.asc >/dev/null
sudo chmod 644 /etc/apt/keyrings/ballast.asc
echo 'deb [signed-by=/etc/apt/keyrings/ballast.asc] https://anur4ag.github.io/ballast/apt stable main' | sudo tee /etc/apt/sources.list.d/ballast.list
sudo apt update
sudo apt install ballast
```

Do not install a second service with `brew services` and do not run `sudo ballast install`.
The package manager owns the binary; Ballast owns the per-user service and hook setup.
If the package repository is unavailable, report that blocker instead of inventing another download source.

## 3. Preview and request consent

```sh
ballast install --dry-run --json
```

Read the plan and explain the detected agents, files, backup paths, preserved hooks, service and required user actions.
Show the diffs when requested and do not expose unrelated private configuration to other services.
Explicitly ask whether the user approves applying this plan, then wait for the answer.
A generic request to set up Ballast does not replace approval of the concrete plan.
If the user wants to choose components, have them run `ballast install` interactively and use `c`.

## 4. Apply the approved plan

Only after the user approves:

```sh
ballast install --yes --json
ballast doctor --json
```

The apply command recomputes the plan from current files, so repeat preview and approval if the configuration or intended scope has changed since the user reviewed it.
Interpret the documented [JSON fields and exit codes](cli.md#installer-json-schema-version-1), including `2` for nothing to change and `5` for partial failure.
Do not use shell `&&` to suppress doctor after exit `2`.
If a sandbox blocks configuration writes or `launchctl`/`systemctl --user`, report the named step and ask for access or an approved run outside the sandbox.
Do not bypass the sandbox by editing files manually.
Run doctor again after any repair.

## 5. Report the result

Tell the user which items were applied or skipped and quote any failed checks and their remedies.
Report `requires_user_action`, `next_steps` and `current_session_needs_restart` honestly.
If Codex hooks are installed, the user must open Codex and approve the Ballast hooks through `/hooks` themselves.
Do not claim that Codex is ready while doctor still reports `action_required` for trust.
Explain that hook changes take effect in new sessions and the current agent session needs restarting when reported.
End with the next action: approve Codex hooks with `/hooks`, or run `ballast top` if approval is not needed.

To undo, preview `ballast uninstall --dry-run --json`, obtain approval, then run `ballast uninstall --yes --json`.
Uninstall resumes frozen work and retains Ballast data by default; `--purge` additionally removes Ballast configuration, state and logs and must be included in the approved preview.
Remove the package afterwards with `brew uninstall ballast` or user-approved `sudo apt remove ballast`.
