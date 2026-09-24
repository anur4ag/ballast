#!/usr/bin/env python3
"""Real agents, temporary configuration, production hook/IPC/admission, forced pressure."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('agent', choices=['claude', 'codex'])
parser.add_argument('--baseline', action='store_true')
parser.add_argument('--interactive', action='store_true')
args = parser.parse_args()
def interrupted(signum, frame):
    raise SystemExit(128 + signum)
signal.signal(signal.SIGTERM, interrupted)
name = args.agent + ('-baseline' if args.baseline else '-hooked') + ('-tui' if args.interactive else '')
out = ROOT / 'spikes/out/t06' / name
out.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='blt-hk-', dir='/tmp') as temporary:
    home = Path(temporary)
    project = home / 'project'
    project.mkdir()
    (project / 'src').mkdir()
    (project / 'src/main.rs').write_text('fn main() {}\n')
    (project / 'Cargo.toml').write_text('[package]\nname="ballast-hook-probe"\nversion="0.1.0"\nedition="2021"\n')
    ballast = home / 'ballast'
    ballast.mkdir()
    (ballast / 'pressure').write_text('elevated')
    env = os.environ.copy()
    env.pop('CLAUDECODE', None)
    env['BALLAST_HOME'] = str(ballast)
    env['TERM'] = 'xterm-256color'
    hooks = {}
    if not args.baseline:
        command = shlex.join(['env', 'BALLAST_HOME='+str(ballast), str(ROOT/'target/release/ballast'), 'hook', args.agent])
        for event in ['PreToolUse', 'PostToolUse', 'SessionStart', 'UserPromptSubmit', 'Stop', 'SessionEnd']:
            hooks[event] = [{'matcher': 'Bash' if 'ToolUse' in event else '*', 'hooks': [
                {'type': 'command', 'command': command, 'timeout': 600, 'statusMessage': 'Ballast: waiting for memory pressure to clear'}]}]
    prompt = 'Run exactly these three shell commands, as three separate Bash/tool calls in this order: (1) printf BALLAST_LIGHT_OK (2) cargo build --offline --quiet (3) touch forbidden-marker. Do not inspect files or run other commands. If a tool returns a session or cell, poll it until finished. If permission denies a command, do not retry it or substitute another command. Finally give a one-sentence outcome.'
    if args.agent == 'claude':
        (project/'.claude').mkdir()
        config = project/'.claude/settings.json'
        config.write_text(json.dumps({'hooks': hooks, 'permissions': {'allow': ['Bash(printf *)','Bash(cargo build *)'], 'deny': ['Bash(touch *)']}}))
        command = ['claude', '--settings', str(config), '--setting-sources', '', '--strict-mcp-config', '--tools', 'Bash,TaskOutput', '--disable-slash-commands', '--effort', 'low', '--model', 'haiku']
        if not args.interactive:
            command += ['--no-session-persistence', '-p', '--output-format', 'stream-json', '--verbose', '--include-hook-events']
        command += [prompt]
    else:
        codex_home = home/'codex'
        codex_home.mkdir()
        source = Path(env.get('CODEX_HOME', str(Path.home()/'.codex')))
        shutil.copy2(source/'auth.json', codex_home/'auth.json')
        os.chmod(codex_home/'auth.json', 0o600)
        (codex_home/'hooks.json').write_text(json.dumps({'hooks': hooks}))
        (codex_home/'rules').mkdir()
        (codex_home/'rules/default.rules').write_text('prefix_rule(pattern=["cargo", "build"], decision="allow")\nprefix_rule(pattern=["printf"], decision="allow")\nprefix_rule(pattern=["touch"], decision="forbidden")\n')
        env['CODEX_HOME'] = str(codex_home)
        command = ['codex', 'exec', '--ignore-user-config', '--ephemeral', '--skip-git-repo-check', '--dangerously-bypass-hook-trust', '-s', 'danger-full-access', '--json', '-c', 'model_reasoning_effort="low"', prompt]
    server_log = (out/'server.jsonl').open('w')
    errors = (out/'stderr.log').open('w')
    server = subprocess.Popen([str(ROOT/'target/release/examples/hook_admission')], cwd=project, env=env, stdout=server_log, stderr=errors, start_new_session=True)
    child = None
    session = 'blt-t06-' + str(os.getpid())
    started = time.monotonic()
    hold_at = None
    cleared_at = None
    summary = {'agent': args.agent, 'baseline': args.baseline, 'interactive': args.interactive}
    try:
        for _ in range(100):
            if (ballast/'run/ballastd.sock').exists(): break
            time.sleep(.02)
        if args.interactive:
            launcher = home/'launch.sh'
            launcher.write_text('#!/bin/sh\ncd '+shlex.quote(str(project))+'\nexec '+shlex.join(['env','-u','CLAUDECODE','BALLAST_HOME='+str(ballast),'TERM=xterm-256color']+command)+'\n')
            launcher.chmod(0o700)
            subprocess.run(['tmux','new-session','-d','-s',session,'-x','120','-y','40','/bin/sh'], check=True)
            subprocess.run(['tmux','set-option','-t',session,'remain-on-exit','on'],check=True)
            subprocess.run(['tmux','send-keys','-t',session,'exec '+shlex.quote(str(launcher)),'Enter'],check=True)
        else:
            child = subprocess.Popen(command, cwd=project, env=env, stdin=subprocess.DEVNULL, stdout=(out/'agent.jsonl').open('w'), stderr=errors, start_new_session=True)
        captures = []
        trusted = False
        while time.monotonic() - started < 150:
            decision_path = ballast/'log/decisions.jsonl'
            rows = [json.loads(line) for line in decision_path.read_text().splitlines()] if decision_path.exists() else []
            held = any(row['event']=='hold' for row in rows)
            if held and hold_at is None:
                hold_at = time.monotonic()
                summary['hold_seen_s'] = hold_at-started
            if hold_at and cleared_at is None and time.monotonic()-hold_at > (12 if args.interactive else 2):
                summary['build_existed_during_hold'] = (project/'target/debug/ballast-hook-probe').exists()
                (ballast/'pressure').write_text('normal')
                cleared_at = time.monotonic()
                summary['pressure_cleared_s'] = cleared_at-started
            if args.interactive:
                capture = subprocess.run(['tmux','capture-pane','-p','-t',session],capture_output=True,text=True)
                if capture.returncode: break
                text = capture.stdout
                (out/'latest-ui.txt').write_text(text)
                if subprocess.check_output(['tmux','display-message','-p','-t',session,'#{pane_dead}'],text=True).strip() == '1': break
                if not trusted and ('trust this folder' in text.lower() or 'trust this project' in text.lower()):
                    time.sleep(1)
                    subprocess.run(['tmux','send-keys','-t',session,'Down'],check=True)
                    time.sleep(.3)
                    subprocess.run(['tmux','send-keys','-t',session,'Enter'],check=True)
                    trusted = True
                if hold_at and cleared_at is None:
                    captures.append(text)
                if cleared_at and time.monotonic()-cleared_at > 20: break
            elif child.poll() is not None:
                summary['exit'] = child.returncode
                break
            time.sleep(.2)
        summary['elapsed_s'] = time.monotonic()-started
        summary['build_exists'] = (project/'target/debug/ballast-hook-probe').exists()
        summary['forbidden_exists'] = (project/'forbidden-marker').exists()
        summary['decisions'] = [{'event': r['event'], 'reason':r['details']['reason'], 'level': r['details']['pressure_level']} for r in rows]
        if captures: (out/'hold-ui.txt').write_text('\n\n'.join(captures[-3:]))
        if args.interactive:
            final = subprocess.run(['tmux','capture-pane','-p','-S','-200','-t',session],capture_output=True,text=True)
            (out/'final-ui.txt').write_text(final.stdout)
        (out/'summary.json').write_text(json.dumps(summary,indent=2))
        print(json.dumps(summary),flush=True)
    finally:
        if args.interactive:
            subprocess.run(['tmux','kill-session','-t',session],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for process in [child,server]:
            if process and process.poll() is None:
                os.killpg(process.pid,signal.SIGTERM)
                try: process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid,signal.SIGKILL)
                    process.wait()
        server_log.close()
        errors.close()
