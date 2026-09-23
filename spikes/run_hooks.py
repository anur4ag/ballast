#!/usr/bin/env python3
"""Bounded CLI experiments, temporary settings/auth copy, owned-process cleanup."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent
OUT = ROOT / 'out'
OUT.mkdir(exist_ok=True)

def run(agent, mode):
    name = agent + '-' + mode + ('-legacy' if os.environ.get('BALLAST_LEGACY_EXEC') else '')
    log = OUT / (name + '-hook.jsonl')
    pidfile = OUT / (name + '-child.json')
    for path in (log, pidfile):
        path.unlink(missing_ok=True)
    with tempfile.TemporaryDirectory(prefix='ballast-spike-') as temporary:
        home = Path(temporary)
        hooks = {}
        if mode != 'stop':
            event = 'PostToolUse' if mode == 'post' else 'PreToolUse'
            hooks[event] = [{'matcher': 'Bash', 'hooks': [{'type': 'command', 'command': shlex.join([sys.executable, str(ROOT / 'hook_probe.py'), mode, str(log)]), 'timeout': 2 if mode == 'timeout' else 120, 'statusMessage': 'Ballast verification hold'}]}]
        config = home / 'settings.json'
        config.write_text(json.dumps({'hooks': hooks}))
        command = shlex.join([sys.executable, str(ROOT / 'stop_probe.py'), str(pidfile)]) if mode == 'stop' else 'sleep 2; printf BALLAST_COMMAND_DONE'
        prompt = ('Execute exactly this one Bash/shell command once: ' + command + '. Set timeout to 3000 milliseconds if supported. Do not inspect files or run other shell commands. If the tool yields or returns a session or cell ID, poll that same execution until it completes; never finish while it is still running. Then report the tool outcome in one short sentence.')
        env = os.environ.copy()
        env.pop('CLAUDECODE', None)
        if agent == 'claude':
            args = ['claude', '-p', prompt, '--settings', str(config), '--setting-sources', '', '--strict-mcp-config', '--tools', 'Bash,TaskOutput', '--allowedTools', 'Bash', '--no-session-persistence', '--disable-slash-commands', '--output-format', 'stream-json', '--verbose', '--include-hook-events']
        else:
            source = Path(env.get('CODEX_HOME', str(Path.home() / '.codex')))
            if (source / 'auth.json').exists():
                shutil.copy2(source / 'auth.json', home / 'auth.json')
                os.chmod(home / 'auth.json', 0o600)
            (home / 'hooks.json').write_text(json.dumps({'hooks': hooks}))
            env['CODEX_HOME'] = str(home)
            args = ['codex', 'exec', '--ignore-user-config', '--ignore-rules', '--ephemeral', '--skip-git-repo-check', '--dangerously-bypass-hook-trust', '-s', 'danger-full-access', '--json', '-c', 'model_reasoning_effort="low"', prompt]
        if agent == 'codex' and os.environ.get('BALLAST_LEGACY_EXEC'):
            args[2:2] = ['--disable', 'unified_exec', '--disable', 'code_mode_host', '-m', 'gpt-5.6-sol']
        started = time.monotonic()
        child = None
        resumed = False
        status = {'agent': agent, 'mode': mode, 'started': time.time(), 'command': args[:-1] if agent == 'codex' else args, 'cwd': temporary}
        def interrupted(signum, frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, interrupted)
        with open(OUT / (name + '.jsonl'), 'w') as output, open(OUT / (name + '.stderr'), 'w') as error:
            process = subprocess.Popen(args, cwd=temporary, env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=error, start_new_session=True)
            try:
                while process.poll() is None and time.monotonic() - started < 180:
                    if pidfile.exists() and child is None:
                        child = json.loads(pidfile.read_text())
                    if child and not resumed and time.time() - child['start'] > 15:
                        try:
                            os.kill(child['pid'], signal.SIGCONT)
                            status['resume'] = {'time': time.time(), 'alive': True}
                        except ProcessLookupError:
                            status['resume'] = {'time': time.time(), 'alive': False}
                        resumed = True
                    time.sleep(.2)
                status['exit'] = process.poll()
                status['elapsed'] = time.monotonic() - started
            finally:
                # All signals target this run's own process group or its recorded child.
                for sig in (signal.SIGCONT, signal.SIGTERM):
                    try:
                        os.killpg(process.pid, sig)
                    except ProcessLookupError:
                        pass
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                if child:
                    try:
                        os.kill(child['pid'], signal.SIGCONT)
                        os.kill(child['pid'], signal.SIGTERM)
                    except ProcessLookupError:
                        pass
        (OUT / (name + '-status.json')).write_text(json.dumps(status, indent=2))
        print(json.dumps(status), flush=True)

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('agent', choices=['claude', 'codex'])
    parser.add_argument('mode', choices=['hold', 'stop', 'timeout', 'crash', 'post'])
    args = parser.parse_args()
    run(args.agent, args.mode)
