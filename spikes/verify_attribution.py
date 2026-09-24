#!/usr/bin/env python3
"""Run isolated real-agent attribution checks; signal only this run's children."""
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / 'target/debug/ballast'
OUT = ROOT / 'spikes/out'
OUT.mkdir(exist_ok=True)


def snapshot(base):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(5)
        sock.connect(str(base / 'run/ballastd.sock'))
        sock.sendall(b'{"version":1,"method":"snapshot"}\n')
        with sock.makefile('rb') as reader:
            return json.loads(reader.readline())['snapshot']


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()


def main():
    with tempfile.TemporaryDirectory(prefix='blt-real-', dir='/tmp') as directory:
        base = Path(directory)
        (base / 'daemon').mkdir()
        (base / 'daemon/config.toml').write_text('mode = "observe"\nnotifications = false\nrecovery_sweep_markers = []\n')
        daemon_env = os.environ.copy()
        daemon_env['BALLAST_HOME'] = str(base / 'daemon')
        daemon = subprocess.Popen([str(BINARY), 'daemon'], env=daemon_env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                                  start_new_session=True)
        agents = []
        detached = {}
        try:
            deadline = time.monotonic() + 15
            while not (base / 'daemon/run/ballastd.sock').exists():
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('daemon did not start')
                time.sleep(.1)
            initial = snapshot(base / 'daemon')
            table = {p['identity']['pid']: p for p in initial['processes']}
            traycer = [a for a in initial['attribution']['agents'] if a['root'] and table.get(table[a['root']['pid']]['ppid'], {}).get('exe', '').endswith('/traycer-host')]
            ps = subprocess.check_output([str(BINARY), 'ps'], env=daemon_env, text=True)
            (OUT / 't04-traycer-attribution.txt').write_text('\n'.join(line for line in ps.splitlines() if line.startswith('AGENT ') and any(a['id'] in line for a in traycer)))
            print(json.dumps({'traycer_spawned_agents': len(traycer), 'kinds': sorted({a['kind'] for a in traycer})}), flush=True)
            for kind in ('claude', 'codex'):
                home = base / kind
                home.mkdir()
                pidfile = home / 'detached.pid'
                script = home / 'child.py'
                script.write_text('import subprocess, pathlib, os, json, sys, time\n'
                                  'time.sleep(2)\n'
                                  'p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(75)"], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n'
                                  f'pathlib.Path({str(pidfile)!r}).write_text(str(p.pid))\n'
                                  f'pathlib.Path({str(home / "markers.json")!r}).write_text(json.dumps({{k:v for k,v in os.environ.items() if k in ["BALLAST_OWNER","CLAUDE_PID","CLAUDE_CODE_SESSION_ID","CODEX_THREAD_ID"]}}))\n')
                command = f'sleep 8; python3 {shlex.quote(str(script))}; sleep 12; printf BALLAST_DONE'
                prompt = ('Execute exactly this one Bash/shell command once: ' + command +
                          '. Do not inspect files or run other commands. If execution yields, poll it until complete. Then answer done.')
                env = os.environ.copy()
                for key in ('CLAUDECODE', 'CLAUDE_CODE_SESSION_ID', 'CLAUDE_PID', 'CODEX_THREAD_ID', 'BALLAST_OWNER', 'BALLAST_OWNER_NAME'):
                    env.pop(key, None)
                env['BALLAST_OWNER'] = f't04-real-{kind}'
                env['BALLAST_OWNER_NAME'] = f'T04 real {kind}'
                if kind == 'claude':
                    config = home / 'settings.json'
                    config.write_text('{"hooks":{}}')
                    args = ['claude', '-p', prompt, '--settings', str(config), '--setting-sources', '', '--strict-mcp-config', '--tools', 'Bash,TaskOutput', '--allowedTools', 'Bash', '--no-session-persistence', '--disable-slash-commands', '--output-format', 'stream-json', '--verbose']
                else:
                    source = Path(env.get('CODEX_HOME', str(Path.home() / '.codex')))
                    if (source / 'auth.json').exists():
                        shutil.copy2(source / 'auth.json', home / 'auth.json')
                        os.chmod(home / 'auth.json', 0o600)
                    env['CODEX_HOME'] = str(home)
                    args = ['codex', 'exec', '--ignore-user-config', '--ignore-rules', '--ephemeral', '--skip-git-repo-check', '-s', 'danger-full-access', '--json', '-c', 'model_reasoning_effort="low"', prompt]
                with open(OUT / f't04-{kind}.jsonl', 'w') as output, open(OUT / f't04-{kind}.stderr', 'w') as error:
                    agent = subprocess.Popen(args, cwd=home, env=env, stdout=output, stderr=error, stdin=subprocess.DEVNULL, start_new_session=True)
                agents.append(agent)
                seen = {}
                started = time.monotonic()
                while time.monotonic() - started < 140:
                    view = snapshot(base / 'daemon')
                    attr = view['attribution']
                    ours = [a for a in attr['agents'] if a['owner_id'] == env['BALLAST_OWNER']]
                    ids = {a['id'] for a in ours}
                    work = [w for w in attr['workloads'] if w['agent_id'] in ids]
                    assigned = [p for p in attr['processes'] if p['agent_id'] in ids]
                    for p in assigned:
                        seen[str(p['identity']['pid'])] = {**p, 'exe': next(x['exe'] for x in view['processes'] if x['identity'] == p['identity'])}
                    if work and 'ps' not in seen:
                        text = subprocess.check_output([str(BINARY), 'ps'], env=daemon_env, text=True)
                        seen['ps'] = '\n'.join(line for line in text.splitlines() if env['BALLAST_OWNER'] in line or any(i in line for i in ids))
                    if pidfile.exists():
                        pid = int(pidfile.read_text())
                        if pid not in detached:
                            detached[pid] = None
                        current = next((p['identity'] for p in view['processes'] if p['identity']['pid'] == pid), None)
                        if current is not None:
                            detached[pid] = current
                    if agent.poll() is not None:
                        time.sleep(1.2)
                        break
                    time.sleep(.5)
                stop(agent)
                final = snapshot(base / 'daemon')['attribution']
                evidence = {'detached_pid': int(pidfile.read_text()) if pidfile.exists() else None, 'markers': json.loads((home / 'markers.json').read_text()) if (home / 'markers.json').exists() else None, 'kind': kind, 'exit': agent.returncode, 'seconds': round(time.monotonic() - started, 1), 'observed': seen, 'detached_attribution': [p for p in final['processes'] if p['identity']['pid'] in detached], 'ended': [a for a in final['agents'] if a['owner_id'] == env['BALLAST_OWNER']]}
                (OUT / f't04-{kind}-attribution.json').write_text(json.dumps(evidence, indent=2))
                print(json.dumps({'kind': kind, 'exit': agent.returncode, 'seconds': evidence['seconds'], 'processes_seen': len(seen), 'workloads_in_ps': 'ps' in seen, 'roles': sorted({v['role'] for v in seen.values() if isinstance(v, dict)})}), flush=True)
        finally:
            for agent in agents:
                stop(agent)
            live = json.loads(subprocess.check_output([str(BINARY), 'debug', 'platform']))['processes']
            live_ids = {p['identity']['pid']: p['identity'] for p in live}
            for pid, identity in detached.items():
                if identity is None or live_ids.get(pid) != identity:
                    continue
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            stop(daemon)


if __name__ == '__main__':
    main()
