#!/usr/bin/env python3
"""Run in a real PTY; trust only the temporary fixture folder when prompted."""
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent
out = root / 'out'
with tempfile.TemporaryDirectory(prefix='ballast-tui-') as directory:
    home = Path(directory)
    original = Path(os.environ.get('CODEX_HOME', str(Path.home()/'.codex')))
    shutil.copy2(original/'auth.json', home/'auth.json')
    os.chmod(home/'auth.json', 0o600)
    command = shlex.join([sys.executable, str(root/'hook_probe.py'), 'hold', str(out/'codex-tui-hook.jsonl')])
    (home/'hooks.json').write_text(json.dumps({'hooks':{'PreToolUse':[{'matcher':'Bash','hooks':[{'type':'command','command':command,'timeout':120,'statusMessage':'Ballast verification hold'}]}]}}))
    env = os.environ.copy()
    env.update(CODEX_HOME=directory, TERM='xterm-256color')
    args = ['codex','--no-daemon','--no-alt-screen','--dangerously-bypass-hook-trust','-a','never','-s','danger-full-access','-C',directory,'Run exactly one shell command: printf BALLAST_TUI_DONE. Wait for it to finish, then say done.']
    process = subprocess.Popen(args, env=env, start_new_session=True)
    try:
        process.wait(timeout=150)
    except subprocess.TimeoutExpired:
        print("Interactive observation deadline reached; cleaning up fixture.")
    finally:
        for sig in (signal.SIGCONT,signal.SIGTERM):
            try:
                os.killpg(process.pid,sig)
            except ProcessLookupError:
                pass
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid,signal.SIGKILL)
            process.wait()
