#!/usr/bin/env python3
"""Scenario 8: real release hooks, with in-process first-response timestamps."""
from concurrent.futures import ThreadPoolExecutor
import json
import math
import os
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import threading
import time

from run_scenarios import BALLAST, active, emit, register, rows, wait_until


def snapshot():
    try:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(.5)
            connection.connect(str(Path(os.environ['BALLAST_HOME'])/'run/ballastd.sock'))
            connection.sendall(b'{"version":1,"method":"snapshot"}\n')
            with connection.makefile('rb') as stream:
                return json.loads(stream.readline())['snapshot']
    except (OSError, KeyError, ValueError):
        return None


def gate(view):
    return bool(view and view['pressure'] and not view['status']['sample_discarded']
                and view['status']['pressure_level']!='normal' and view['status']['batch_running'])


def concurrent(plan, path):
    output = Path(plan['latency_dir'])
    view = snapshot()
    foreign = bool(view and any(a['session_id']==plan['session'] and a['root'] for a in view['attribution']['agents']))
    expected = gate(view) if path=='hold' else foreign
    pid = int(Path(plan['agent_pid_file']).read_text())
    command = {'deny_pid':f'kill {pid}', 'deny_argv':f'pkill -f {shlex.quote(Path(plan["dir"]).name)}', 'hold':'cargo build --release'}[path]
    barrier = threading.Barrier(10)

    def call(index):
        audit = output/f'{path}-{index}.jsonl'
        env = os.environ.copy()
        env['BALLAST_HOOK_AUDIT'] = str(audit)
        env['DYLD_INSERT_LIBRARIES' if sys.platform=='darwin' else 'LD_PRELOAD'] = plan['audit_library']
        payload = {'session_id':plan['caller_session'], 'cwd':plan['dir'], 'hook_event_name':'PreToolUse',
                   'tool_name':'Bash', 'tool_input':{'command':command}}
        barrier.wait()
        began = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
        child = subprocess.Popen([str(BALLAST),'hook','claude'], env=env, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            child.stdin.write(json.dumps(payload))
            child.stdin.close()
        except BrokenPipeError:
            pass
        reply = None
        until = time.monotonic()+1
        while time.monotonic()<until:
            records = rows(audit)
            reply = next((r for r in records if r['event']=='first_response'), None)
            if reply or child.poll() is not None:
                break
            time.sleep(.002)
        held = reply and reply['decision']=='hold'
        if held:
            child.terminate()
        try:
            child.wait(timeout=1)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
        stdout, stderr = child.stdout.read(), child.stderr.read()
        records = rows(audit)
        if not any(r['event']=='loaded' for r in records):
            raise RuntimeError('hook audit library did not load')
        reply = next((r for r in records if r['event']=='first_response'), None)
        decision = reply['decision'] if reply else None
        denied = '"deny"' in stdout
        if not plan['enforced']:
            verdict = 'baseline: daemon absent'
        elif path=='hold' and decision=='admit':
            verdict = 'admitted, precondition not reached'
        elif not expected:
            verdict = 'precondition not reached'
        elif path=='hold' and decision=='hold':
            verdict = 'held'
        elif path!='hold' and denied:
            verdict = 'denied'
        else:
            verdict = 'fail-open'
        return dict(index=index, command=command, expected=expected, decision=decision, verdict=verdict,
                    first_response_ms=reply['elapsed_ns']/1e6 if reply else None,
                    spawn_to_response_ms=(reply['ns']-began)/1e6 if reply else None,
                    exit_ms=(time.clock_gettime_ns(time.CLOCK_MONOTONIC)-began)/1e6,
                    exit_code=child.returncode, cancelled_after_hold=bool(held), stdout=stdout, stderr=stderr)

    with ThreadPoolExecutor(max_workers=10) as pool:
        results = list(pool.map(call, range(10)))
    latencies = sorted(r['first_response_ms'] for r in results if r['first_response_ms'] is not None)
    # Missing first responses occupy the upper quantiles rather than disappearing from p99.
    quantiles = {}
    for name, fraction in [('p50',.5),('p95',.95),('p99',.99)]:
        index = math.ceil(len(results)*fraction)-1
        quantiles[name] = latencies[index] if index<len(latencies) else None
    eligible = [r for r in results if plan['enforced'] and r['expected'] and r['verdict']!='admitted, precondition not reached']
    summary = dict(path=path, samples=results, first_response_ms=quantiles,
                   fail_open_rate=sum(r['verdict']=='fail-open' for r in eligible)/len(eligible) if eligible else None,
                   eligible_calls=len(eligible), expected_gate=expected,
                   no_response=sum(r['first_response_ms'] is None for r in results),
                   observed_pressure=view['status']['pressure_level'] if view else None,
                   batch_running=view['status']['batch_running'] if view else None)
    (output/f'{path}.json').write_text(json.dumps(summary,indent=2)+'\n')
    emit(Path(plan['events']), event='hook_burst', hook_path=path, first_response_ms=quantiles,
         fail_open_rate=summary['fail_open_rate'], expected=expected)


def main():
    plan = json.loads(Path(sys.argv[1]).read_text())
    register(plan, os.getpid())
    output = Path(plan['latency_dir'])
    output.mkdir(exist_ok=True)
    if plan['dry']:
        wait_until(plan, plan['start']+.25)
    else:
        wait_until(plan, plan['start']+10)
        if plan['scenario']==1 and plan['enforced']:
            while active(plan) and time.time()<plan['deadline']-5 and not gate(snapshot()):
                time.sleep(.25)
        elif plan['scenario']==1:
            wait_until(plan, plan['start']+60)
    concurrent(plan, 'deny_pid')
    concurrent(plan, 'deny_argv')
    if not plan['dry'] and plan['enforced']:
        while active(plan) and time.time()<plan['deadline']-3 and not gate(snapshot()):
            time.sleep(.25)
    concurrent(plan, 'hold')


if __name__=='__main__':
    main()
