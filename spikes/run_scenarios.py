#!/usr/bin/env python3
"""Bounded native scenarios. See spikes/SCENARIOS.md before using --pressure."""
import argparse
from collections import deque
import json
import hashlib
import math
import os
from pathlib import Path
import platform
import plistlib
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
NATIVE = ROOT / 'target/release/examples/scenario_native'
BALLAST = ROOT / 'target/release/ballast'
MIB = 1024 * 1024


def rows(path):
    if not path.exists():
        return []
    # Concurrent appenders can leave an incomplete last line until the next read.
    return [json.loads(line) for line in path.read_text().splitlines(keepends=True)
            if line.endswith('\n')]


def new_rows(path, offset):
    if not path.exists():
        return [], offset
    with path.open('rb') as f:
        f.seek(offset)
        data = f.read()
    complete, newline, _ = data.rpartition(b'\n')
    if not newline:
        return [], offset
    return [json.loads(line) for line in complete.splitlines()], offset+len(complete)+1


def emit(path, **data):
    with path.open('a') as f:
        f.write(json.dumps(dict(time_ms=time.time_ns() // 1_000_000, **data)) + '\n')


def register(plan, pid):
    subprocess.run([str(NATIVE), 'register', plan['registry'], str(pid)], check=True)


def active(plan):
    return time.monotonic() < plan['deadline_monotonic'] and not Path(plan['stop']).exists()


def wait_until(plan, deadline):
    while active(plan) and time.time() < deadline:
        time.sleep(.05)


def worker(plan, kind, index):
    register(plan, os.getpid())
    emit(Path(plan['events']), event='worker_start', kind=kind, pid=os.getpid())
    wait_until(plan, plan['start'])
    load_started = time.monotonic()
    emit(Path(plan['events']), event='load_started', kind=kind, pid=os.getpid())
    memory = []
    seed = os.urandom(MIB)
    cap = plan['memory_mib'] // (4 if plan['scenario'] == 1 else 1)
    chunk = 1 if plan['dry'] else (64 if plan['scenario']==2 else 4)
    allocated = 0

    def grow(target):
        nonlocal allocated
        while allocated < min(target, cap) and active(plan):
            size = min(chunk, cap - allocated)
            # The spike measures sequential page writes, not random-byte generation throughput.
            memory.append(bytearray(seed)*size if plan['scenario']==2 else bytearray(os.urandom(size*MIB)))
            allocated += size
            if allocated == cap:
                emit(Path(plan['events']), event='allocation_target', kind=kind, pid=os.getpid(), allocated_mib=allocated, elapsed_s=time.monotonic()-load_started)

    def touch():
        for block in memory:
            if not active(plan):
                break
            for i in range(0, len(block), 4096):
                block[i] ^= 1

    if kind == 'launcher':
        args = [sys.executable, __file__, '--worker', 'escaped', '--plan', plan['file']]
        if sys.platform == 'darwin':
            app = Path(plan['dir']) / 'Scenario.app'
            (app / 'Contents/MacOS').mkdir(parents=True)
            shutil.copy2(NATIVE, app / 'Contents/MacOS/scenario')
            (app / 'Contents/Info.plist').write_bytes(plistlib.dumps({
                'CFBundleExecutable': 'scenario', 'CFBundleIdentifier': 'test.ballast.' + plan['session'],
                'CFBundleName': 'Ballast scenario', 'LSUIElement': True}))
            subprocess.run(['/usr/bin/open', '-n', '-a', str(app), '--args', 'exec', *args], env={'PATH':'/usr/bin:/bin','HOME':plan['dir']}, check=True)
        else:
            import shlex
            escaped = ['setsid', 'env', '-i', 'PATH=/usr/bin:/bin', 'HOME=' + plan['dir'], *args]
            subprocess.run(['/bin/sh', '-c', shlex.join(escaped) + ' </dev/null >/dev/null 2>&1 &'], check=True)
        return
    if kind == 'mcp':
        # Same group and direct ancestry preserve the agent-internal classification.
        child = subprocess.Popen([sys.executable, __file__, '--worker', 'memory', '--plan', plan['file']])
        try:
            while active(plan) and child.poll() is None:
                time.sleep(.1)
        finally:
            if child.poll() is None:
                child.terminate()
            child.wait()
    elif kind == 'client':
        try:
            while active(plan) and not Path(plan['port']).exists():
                time.sleep(.05)
            port = int(Path(plan['port']).read_text())
            while active(plan):
                with socket.create_connection(('127.0.0.1', port), timeout=1) as connection:
                    connection.settimeout(1)
                    connection.sendall(b'ping')
                    if connection.recv(4) != b'pong':
                        raise RuntimeError('bad server response')
                time.sleep(.02)
        except (OSError, ValueError, RuntimeError) as error:
            emit(Path(plan['events']), event='work_failed', kind=kind, error=str(error))
            return
    elif kind == 'server':
        with socket.socket() as server:
            server.bind(('127.0.0.1', 0))
            server.listen()
            server.settimeout(.2)
            Path(plan['port']).write_text(str(server.getsockname()[1]))
            while active(plan):
                try:
                    connection, _ = server.accept()
                except socket.timeout:
                    continue
                with connection:
                    connection.settimeout(1)
                    connection.recv(4)
                    grow(allocated + chunk)
                    until = time.monotonic() + (.001 if plan['dry'] else .02)
                    while time.monotonic() < until:
                        pass
                    try:
                        connection.sendall(b'pong')
                    except OSError:
                        pass
    elif kind == 'disk':
        block = seed * (1 if plan['dry'] else 8)
        written = 0
        disk_path = Path(plan['dir']) / 'io-load'
        with disk_path.open('w+b', buffering=0) as f:
            while active(plan) and written < plan['write_mib'] * MIB:
                f.write(block)
                os.fsync(f.fileno())
                written += len(block)
                if f.tell() >= plan['file_mib'] * MIB:
                    f.seek(0)
                wait_until(plan, plan['start'] + (plan['deadline']-plan['start']) * written/(plan['write_mib']*MIB))
        emit(Path(plan['events']), event='disk_written', bytes=written)
    elif kind == 'cpu':
        value = 1
        while active(plan):
            for _ in range(10000):
                value = (value * 1664525 + 1013904223) & 0xffffffff
    else:
        ramp = 1 if plan['dry'] else (3 if plan['scenario'] == 2 else 40)
        while active(plan):
            elapsed = time.monotonic()-load_started
            grow(math.ceil(cap * min(1, elapsed / ramp)))
            touch()
            if plan.get('recovery_cycle') and allocated == cap and time.time() >= plan['finish_after'] and active(plan):
                emit(Path(plan['events']), event='task_complete', pid=os.getpid(), allocated_mib=allocated, target_mib=cap)
                break
            time.sleep(.02)
    emit(Path(plan['events']), event='worker_done', kind=kind, pid=os.getpid(), allocated_mib=allocated, active_s=time.monotonic()-load_started)


def hook(plan, event, command=''):
    payload = {'session_id': plan['session'], 'cwd': plan['dir'], 'hook_event_name': event,
               'tool_name': 'Bash', 'tool_input': {'command': command}, 'tool_use_id': str(uuid.uuid4())}
    begin = time.monotonic()
    reply = subprocess.run([str(BALLAST), 'hook', 'claude'], input=json.dumps(payload),
                           text=True, capture_output=True, timeout=max(1, plan['deadline'] - time.time()))
    emit(Path(plan['events']), event='hook', hook=event, elapsed_s=time.monotonic()-begin,
         stdout=reply.stdout, stderr=reply.stderr, code=reply.returncode)
    return reply.returncode == 0 and '"deny"' not in reply.stdout


def probe(plan):
    buffer = bytearray(8*MIB)
    for i in range(0, len(buffer), 4096):
        buffer[i] = 1
    while active(plan):
        sample = time.perf_counter()
        time.sleep(.01)
        scheduling = max(0, time.perf_counter() - sample - .01)
        start = time.perf_counter()
        for i in range(0, len(buffer), 4096):
            buffer[i] ^= 1
        emit(Path(plan['probe']), scheduling_ms=scheduling*1000, touch_ms=(time.perf_counter()-start)*1000)
        time.sleep(max(0, .05-(time.perf_counter()-sample)))


def watchdog(plan):
    # Independent of the runner and load groups, including when the runner is killed.
    cancel = Path(plan['dir'])/'WATCHDOG_DONE'
    while time.monotonic() < plan['deadline_monotonic'] + 10:
        if cancel.exists():
            return
        time.sleep(.1)
    Path(plan['stop']).touch()
    combined = Path(plan['dir'])/'emergency.jsonl'
    combined.write_text(Path(plan['registry']).read_text()+Path(plan['controls']).read_text())
    for action in ('resume', 'term', 'kill'):
        subprocess.run([str(NATIVE), 'signal', str(combined), action], timeout=5, check=True)
        time.sleep(.5)
    verify = subprocess.run([str(NATIVE), 'verify', str(combined)], capture_output=True, text=True, timeout=5)
    emit(Path(plan['events']), event='watchdog_cleanup', verified=verify.returncode==0, remaining=verify.stdout)
    if verify.returncode == 0:
        shutil.rmtree(plan['dir'])


def percentiles(samples):
    ordered = sorted(samples)
    if not ordered:
        return None
    return {key: ordered[min(len(ordered)-1, math.ceil(len(ordered)*q)-1)]
            for key, q in [('p50', .5), ('p99', .99), ('max', 1)]}


def run_one(args, scenario, enforced, hardware):
    label = f'8-{"memory" if scenario==1 else "cpu"}' if args.latency else str(scenario)
    output = args.output / f'{label}-{"enforced" if enforced else "baseline"}'
    output.mkdir(parents=True, exist_ok=False)
    if shutil.disk_usage(tempfile.gettempdir()).free < (768 if args.pressure else 256)*MIB:
        raise RuntimeError('insufficient free space for the bounded fixture')
    temp = Path(tempfile.mkdtemp(prefix='ballast-scenario-'))
    plan = dict(file=str(temp/'plan.json'), dir=str(temp), registry=str(temp/'registry.jsonl'),
                stop=str(temp/'STOP'), controls=str(temp/'controls.jsonl'), events=str(output/'events.jsonl'), probe=str(output/'probe.jsonl'),
                port=str(temp/'port'), session=uuid.uuid4().hex, scenario=scenario, dry=not args.pressure, recovery_cycle=args.recovery_cycle,
                hooks=sys.platform=='linux', latency=args.latency, enforced=enforced, cpu_hold_memory=args.latency and scenario==3 and sys.platform=='linux', cores=min(os.cpu_count() or 1, 10) if args.pressure else 2,
                memory_mib=(min(10240 if args.mac_memory_rerun else 7168, int(hardware['total_memory_bytes']/MIB*.96)) if args.pressure else 32),
                write_mib=8192 if args.pressure else 16, file_mib=128 if args.pressure else 8,
                swap_growth_mib=2048 if args.mac_memory_rerun else 384)
    plan.update(caller_session=uuid.uuid4().hex, agent_pid_file=str(temp/'agent.pid'), latency_dir=str(output/'hooks'),
                audit_library=str(ROOT/('target/hook_response_audit.dylib' if sys.platform=='darwin' else 'target/hook_response_audit.so')))
    if args.memory_mib is not None:
        if not 1 <= args.memory_mib <= plan['memory_mib']:
            raise ValueError('--memory-mib exceeds the host-scaled hard cap')
        plan['memory_mib'] = args.memory_mib
    duration = (60 if scenario in (3, 7) else 80) if args.pressure else 3
    if args.recovery_cycle:
        duration = 720 if args.pressure else 8
    plan['deadline_monotonic'] = time.monotonic()+duration+(3 if args.pressure else 1)
    plan.update(start=time.time() + (3 if args.pressure else 1), deadline=time.time()+duration+ (3 if args.pressure else 1))
    plan['finish_after'] = plan['start'] + (90 if args.pressure else 3)
    Path(plan['file']).write_text(json.dumps(plan))
    shutil.copy2(plan['file'], output/'plan.json')
    Path(plan['registry']).touch()
    Path(plan['controls']).touch()
    home = temp/'home'
    home.mkdir()
    marker = 'BALLAST_SCENARIO_' + plan['session'].upper()
    ballast_home = temp/'ballast'
    ballast_home.mkdir()
    (ballast_home/'config.toml').write_text('notifications = false\nmode = "enforce"\nrecovery_sweep_markers = ["'+marker+'"]\n[[markers]]\nkey = "'+marker+'"\nlevel = "owner"\n')
    env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'HOME': str(home), 'BALLAST_HOME': str(ballast_home),
           'CLAUDE_CODE_SESSION_ID': plan['session'], marker: plan['session']}
    claude = temp/'claude'
    shutil.copy2(NATIVE, claude)
    children = []
    handles = []
    emergency = None
    initial_pressure = json.loads(subprocess.check_output([str(NATIVE), 'pressure'], text=True))
    verified = False
    code = None
    daemon = None
    caller = None
    subprocess.run([str(NATIVE), 'register', plan['controls'], str(os.getpid())], check=True)
    guard = subprocess.Popen([sys.executable, __file__, '--watchdog', '--plan', plan['file']], start_new_session=True)
    def launch(command, name, session=None, **kw):
        file = (output/name).open('w')
        handles.append(file)
        child_env = env.copy()
        if session:
            child_env['CLAUDE_CODE_SESSION_ID'] = session
        if str(command[0]) != str(claude):
            child_env.pop('CLAUDE_CODE_SESSION_ID', None)
            child_env.pop(marker, None)
        error_file = (output/(name+'.stderr')).open('w')
        handles.append(error_file)
        child = subprocess.Popen([str(x) for x in command], env=child_env, stdout=file, stderr=error_file, **kw)
        children.append(child)
        subprocess.run([str(NATIVE), 'register', plan['controls'], str(child.pid)], check=True)
        return child
    try:
        daemon = None
        if enforced and sys.platform == 'linux':
            daemon = launch([BALLAST, 'daemon'], 'daemon.log', start_new_session=True)
            until = time.monotonic()+5
            while not (ballast_home/'run/ballastd.sock').exists() and time.monotonic()<until:
                if daemon.poll() is not None:
                    raise RuntimeError('daemon failed to start')
                time.sleep(.05)
        mode = ('daemon' if sys.platform=='linux' else 'scoped_ipc' if args.latency else 'scoped') if enforced else 'baseline'
        monitor = launch([NATIVE, 'monitor', plan['registry'], plan['stop'], mode, str(duration+15)], 'trace.jsonl')
        foreground = launch([sys.executable, __file__, '--probe', '--plan', plan['file']], 'probe.log')
        root = launch([claude, 'agent', sys.executable, __file__, plan['file']], 'agent.log', start_new_session=True)
        register(plan, root.pid)
        Path(plan['agent_pid_file']).write_text(str(root.pid))
        if args.latency:
            caller = launch([claude,'exec',sys.executable,ROOT/'spikes/hook_latency.py',plan['file']], 'hooks.log', session=plan['caller_session'], start_new_session=True)
        trace_offset = probe_offset = event_offset = 0
        completed_tasks = set()
        samples = deque(maxlen=2)
        latest = None
        while active(plan):
            if args.stop_file and args.stop_file.exists():
                emergency = 'external stop file'
            observations, trace_offset = new_rows(output/'trace.jsonl', trace_offset)
            new_samples, probe_offset = new_rows(Path(plan['probe']), probe_offset)
            samples.extend(new_samples)
            if observations:
                latest = observations[-1]
            if latest:
                swap = latest['inputs'].get('swap_used_bytes') or 0
                if swap - (initial_pressure.get('swap_used_bytes') or 0) > plan['swap_growth_mib']*MIB:
                    emergency = f"swap growth >{plan['swap_growth_mib']} MiB"
            if len(samples) >= 2 and all(s['scheduling_ms'] > 1000 for s in samples):
                emergency = 'two scheduling samples >1000 ms'
            if any(c.poll() is not None for c in (monitor, foreground, root)) or (daemon is not None and daemon.poll() is not None):
                emergency = 'measurement, agent or daemon exited early'
            if caller is not None and caller.poll() not in (None, 0):
                emergency = 'hook benchmark exited early'
            if args.recovery_cycle:
                updates, event_offset = new_rows(Path(plan['events']), event_offset)
                completed_tasks.update(e['pid'] for e in updates if e['event']=='task_complete')
                if len(completed_tasks)==4 and latest and not latest['frozen']:
                    break
            if emergency:
                break
            time.sleep(.1)
        code = root.poll()
    finally:
        measurement_end_ms = time.time_ns() // 1_000_000
        Path(plan['stop']).touch()
        # Resume first, letting roots reap their children before terminating control processes.
        subprocess.run([str(NATIVE), 'signal', plan['registry'], 'resume'], check=True)
        for child in children:
            if child is not daemon:
                try:
                    child.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    child.terminate()
        for action in ('term', 'kill'):
            subprocess.run([str(NATIVE), 'signal', plan['registry'], action], check=True)
            time.sleep(.2)
        for child in children:
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        verification = subprocess.run([str(NATIVE), 'verify', plan['registry']], capture_output=True, text=True)
        verified = verification.returncode == 0
        (output/'cleanup.json').write_text(json.dumps({'verified': verified, 'remaining': verification.stdout, 'error': verification.stderr}))
        for f in handles:
            f.close()
        for name in ('registry.jsonl', 'plan.json'):
            shutil.copy2(temp/name, output/name)
        decisions = ballast_home/'log/decisions.jsonl'
        if decisions.exists():
            shutil.copy2(decisions, output/'decisions.jsonl')
        (temp/'WATCHDOG_DONE').touch()
        guard.wait(timeout=2)
        shutil.rmtree(temp)
    samples = rows(output/'probe.jsonl')
    decisions = rows(output/'decisions.jsonl')
    actions = [r for r in decisions if r.get('event') in ('freeze', 'hold', 'throttle')]
    trace = rows(output/'trace.jsonl')
    events = rows(output/'events.jsonl')
    workloads = max((len(t['attribution']['workloads']) for t in trace), default=0)
    roles = {str(e['pid']): sorted({p['role'] for t in trace for p in t['attribution']['processes'] if p['identity']['pid']==e['pid']})
             for e in events if e['event']=='worker_start'}
    started = [e['kind'] for e in events if e['event']=='worker_start']
    code = root.returncode
    expected_workers = {1:4, 2:1, 3:plan['cores'], 4:2, 5:2, 6:2, 7:1}[scenario]
    held = any(a['event']=='hold' for a in actions) and len(started)<expected_workers
    result = dict(scenario=8 if args.latency else scenario, load_scenario=scenario, enforced=enforced, dry=plan['dry'], host=platform.platform(),
                  scope='production daemon and hooks' if sys.platform=='linux' else 'owned Observer/Attributor/Guardian with IPC/Admission' if args.latency else 'owned Observer/Attributor/Guardian; no admission hooks',
                  plan=plan, builds=args.builds, hardware=hardware, initial_pressure=initial_pressure, probe={k:percentiles([s[k] for s in samples]) for k in ('scheduling_ms','touch_ms')},
                  first_action=actions[0] if actions else None,
                  first_action_s=(actions[0]['timestamp_ms']/1000 - plan['start']) if actions else None,
                  outcome='aborted' if emergency else 'held at deadline' if held else 'failed' if any(e['event']=='work_failed' for e in events) or code not in (None, 0) else ('paused/resumed' if any(t['frozen'] for t in trace) else 'completed'),
                  agent_exit=code, emergency=emergency, cleanup_verified=verified, temp_removed=not temp.exists(),
                  workloads_seen=workloads, worker_roles=roles, workers_started=started,
                  peak_agent_memory_bytes=max((sum(a['memory']['bytes'] for a in t['attribution']['agents']) for t in trace), default=0),
                  peak_owned_memory_bytes=max((sum((p.get('metrics') or {}).get('memory_bytes',0) for p in t.get('processes',[])) for t in trace), default=0),
                  pressure_levels=sorted({str(t['level']) for t in trace}), samples=len(samples))
    if args.recovery_cycle:
        timeline = [e for e in decisions if e['event'] in ('freeze','resume','pressure_transition') and e['timestamp_ms'] <= measurement_end_ms]
        result['recovery_cycle'] = dict(complete=len(completed_tasks)==4, measurement_end_ms=measurement_end_ms,
                                      completions=[e for e in events if e['event']=='task_complete' and e['time_ms']<=measurement_end_ms],
                                      timeline=timeline)
        if not emergency and len(completed_tasks)!=4:
            result['outcome'] = 'task incomplete at deadline'
    if args.latency:
        result['hook_latency'] = {path:json.loads((output/'hooks'/f'{path}.json').read_text()) for path in ('deny_pid','deny_argv','hold') if (output/'hooks'/f'{path}.json').exists()}
        result['caller_exit'] = caller.returncode if caller else None
    (output/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(f'{label} {"enforced" if enforced else "baseline"}: {result["outcome"]}; cleanup={verified}; emergency={emergency}', flush=True)
    if not verified:
        raise RuntimeError('cleanup could not be verified; see '+str(output))
    if emergency:
        raise RuntimeError('measurement aborted: '+str(output))
    if args.latency and (len(result['hook_latency']) != 3 or result['caller_exit'] != 0):
        raise RuntimeError('hook measurement incomplete: '+str(output))
    if plan['dry'] and args.latency and enforced:
        for path, measured in result['hook_latency'].items():
            if path.startswith('deny'):
                assert all(s['expected'] and s['decision']=='deny' and s['verdict']=='denied' for s in measured['samples']), measured
            else:
                assert all(s['decision'] in ('admit', 'hold') for s in measured['samples']), measured
    if plan['dry'] and (result['outcome'] != 'completed' or not samples):
        raise RuntimeError('tiny wiring check did not complete: '+str(output))
    return result


def settle():
    previous, quiet = None, 0
    for _ in range(30):
        now = time.monotonic()
        current = json.loads(subprocess.check_output([str(NATIVE), 'pressure'], text=True))
        normal = current.get('kernel_pressure_level') in (None, 1)
        normal &= (current.get('psi_some_avg10') or 0)<5 and (current.get('psi_full_avg10') or 0)<1
        if previous and current.get('swapouts') is not None:
            elapsed, old = previous
            rate = max(0, current['swapouts']-old['swapouts'])*current['page_size']/MIB/(now-elapsed)
            normal &= rate<32
            normal &= (current.get('swap_used_bytes') or 0) <= (old.get('swap_used_bytes') or 0)
        quiet = quiet+1 if normal else 0
        if quiet>=3:
            return
        previous = now, current
        time.sleep(2)
    raise RuntimeError('host did not settle within 60 s; remaining pressure runs skipped')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--recovery-cycle', action='store_true', help='scenario 1 enforce only: wait for natural recovery and fixed allocation tasks')
    parser.add_argument('--scenario', type=int, choices=range(1,9))
    parser.add_argument('--output', type=Path)
    parser.add_argument('--mode', choices=('baseline', 'enforced', 'both'), default='both')
    parser.add_argument('--memory-mib', type=int, help='lower the host-scaled memory cap')
    parser.add_argument('--pressure', action='store_true')
    parser.add_argument('--mac-approved', action='store_true')
    parser.add_argument('--mac-memory-rerun', action='store_true', help='separately approved Mac memory-only run: 10 GiB cap and 2 GiB swap-growth stop')
    parser.add_argument('--stop-file', type=Path)
    parser.add_argument('--worker')
    parser.add_argument('--index', type=int, default=0)
    parser.add_argument('--hook', action='store_true')
    parser.add_argument('--probe', action='store_true')
    parser.add_argument('--watchdog', action='store_true')
    parser.add_argument('--plan', type=Path)
    args = parser.parse_args()
    if args.plan:
        plan = json.loads(args.plan.read_text())
        if args.watchdog:
            watchdog(plan)
        elif args.worker:
            worker(plan, args.worker, args.index)
        elif args.hook:
            session = Path(plan['dir'])/'SESSION_STARTED'
            if not session.exists():
                hook(plan, 'SessionStart')
                session.touch()
            if not hook(plan, 'PreToolUse', 'cargo build # bounded scenario'):
                sys.exit(1)
        elif args.probe:
            probe(plan)
        return
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    if args.mac_memory_rerun and (sys.platform != 'darwin' or not args.pressure or not args.mac_approved or args.scenario not in (1, 2, 8)):
        parser.error('--mac-memory-rerun requires approved Mac pressure and --scenario 1, 2 or 8')
    if args.pressure:
        if sys.platform == 'darwin' and not args.mac_approved:
            parser.error('macOS pressure requires explicit user approval, then --mac-approved')
        if sys.platform == 'linux':
            # Check the actual guest identity, not an environment override.
            hostname = subprocess.check_output(['hostname'], text=True).strip()
            if hostname != 'lima-ballast-platform':
                parser.error('Linux pressure is restricted to the disposable lima-ballast-platform VM')
    if args.recovery_cycle:
        if args.scenario not in (None, 1):
            parser.error('--recovery-cycle is scenario 1 only')
        args.scenario = 1
    if not args.output:
        parser.error('--output is required')
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    hardware = json.loads(subprocess.check_output([str(NATIVE), 'pressure'], text=True))
    args.builds = {}
    for binary in (NATIVE, BALLAST):
        with binary.open('rb') as f:
            args.builds[binary.name] = hashlib.file_digest(f, 'sha256').hexdigest()
    args.builds['runner'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    cap = min(10240 if args.mac_memory_rerun else 7168, int(hardware['total_memory_bytes']/MIB*.96)) if args.pressure else 32
    if args.memory_mib is not None and not 1 <= args.memory_mib <= cap:
        parser.error('--memory-mib exceeds the host-scaled hard cap')
    results = []
    for scenario in ([args.scenario] if args.scenario else range(1,9)):
        args.latency = scenario==8
        if args.latency:
            library = ROOT/('target/hook_response_audit.dylib' if sys.platform=='darwin' else 'target/hook_response_audit.so')
            flags = ['-dynamiclib'] if sys.platform=='darwin' else ['-shared','-fPIC','-ldl']
            subprocess.run(['cc','-std=c11','-O2','-Wall','-Wextra','-Werror',*flags,str(ROOT/'examples/hook_response_audit.c'),'-o',str(library)],check=True)
        for load in ([1] if args.mac_memory_rerun and args.latency else [1,3] if args.latency else [scenario]):
            for enforced in ([True] if args.recovery_cycle else [args.mode=='enforced'] if args.mode!='both' else (False, True)):
                if args.pressure:
                    settle()
                results.append(run_one(args, load, enforced, hardware))
    table = ['| Scenario | Mode | Sleep p99 ms | Touch p99 ms | First action s | Outcome | Cleanup |', '| --- | --- | ---: | ---: | ---: | --- | --- |']
    for r in results:
        sleep = r['probe']['scheduling_ms']
        touch = r['probe']['touch_ms']
        sleep_p99 = f"{sleep['p99']:.3f}" if sleep else 'no samples'
        touch_p99 = f"{touch['p99']:.3f}" if touch else 'no samples'
        table.append(f'| {r["scenario"]} | {"enforce" if r["enforced"] else "baseline"} | {sleep_p99} | {touch_p99} | {r["first_action_s"]} | {r["outcome"]} | {r["cleanup_verified"]} |')
    hook_rows = [r for r in results if r.get('hook_latency')]
    if hook_rows:
        table += ['', '| Load | Mode | Path | p50 ms | p95 ms | p99 ms | Fail-open rate | Eligible calls |', '| --- | --- | --- | --- | --- | --- | --- | --- |']
        for r in hook_rows:
            for path,h in r['hook_latency'].items():
                q=h['first_response_ms']
                table.append(f'| {r["load_scenario"]} | {r["enforced"]} | {path} | {q["p50"]} | {q["p95"]} | {q["p99"]} | {h["fail_open_rate"]} | {h["eligible_calls"]} |')
    (args.output/'summary.md').write_text('\n'.join(table)+'\n')


if __name__ == '__main__':
    main()
