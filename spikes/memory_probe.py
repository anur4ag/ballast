#!/usr/bin/env python3
"""Bounded four-build Mac probe with owned process-group cleanup and raw JSONL."""
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent
OUT = ROOT / 'out'

def snapshot():
    start = time.monotonic()
    vm = subprocess.check_output(['vm_stat'], text=True, timeout=3)
    pages = {k: int(v) for k, v in re.findall(r'^([^:\n]+):\s+(\d+)\.', vm, re.M)}
    level = int(subprocess.check_output(['sysctl', '-n', 'kern.memorystatus_vm_pressure_level'], timeout=3))
    launch = time.monotonic()
    subprocess.run(['/usr/bin/true'], check=True, timeout=3)
    return {'time': time.time(), 'pressure': level, 'page_size': int(re.search(r'page size of (\d+)', vm)[1]), 'pageouts': pages['Pageouts'], 'swapouts': pages['Swapouts'], 'compressed_pages': pages['Pages occupied by compressor'], 'launch_ms': (time.monotonic()-launch)*1000, 'sample_ms': (time.monotonic()-start)*1000}

def cleanup(processes):
    for p in processes:
        for sig in (signal.SIGCONT, signal.SIGTERM):
            try:
                os.killpg(p.pid, sig)
            except ProcessLookupError:
                pass
    for p in processes:
        try:
            p.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()

def main():
    OUT.mkdir(exist_ok=True)
    source = OUT / 'heavy.cpp'
    with source.open('w') as f:
        f.write('template<int N> struct S { int x[N % 31 + 1]; int f(){return N;} };\n')
        for i in range(500000):
            f.write(f'template struct S<{i}>;\n')
    processes = []
    handles = []
    start = time.monotonic()
    critical_since = None
    frozen = False
    launched = False
    released = False
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    # Independent watchdog survives a stalled sampler and kills only our process groups.
    watchdog = os.fork()
    if watchdog == 0:
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        time.sleep(185)
        try:
            os.kill(os.getppid(), signal.SIGTERM)
        except ProcessLookupError:
            pass
        os._exit(0)
    try:
        with (OUT / 'mac-memory.jsonl').open('w') as log:
            while time.monotonic() - start < 180:
                elapsed = time.monotonic() - start
                if elapsed >= 15 and not launched:
                    for i in range(4):
                        handle = (OUT / f'compile-{i}.log').open('w')
                        handles.append(handle)
                        processes.append(subprocess.Popen(['clang++', '-O2', '-c', '-std=c++17', str(source), '-o', str(OUT / f'compile-{i}.o')], stdout=handle, stderr=handle, start_new_session=True))
                    launched = True
                row = snapshot()
                row['elapsed'] = elapsed
                row['phase'] = 'baseline' if not launched else ('recovery' if released else ('frozen' if frozen else 'builds'))
                alive = [p for p in processes if p.poll() is None]
                row['processes'] = []
                groups = {p.pid for p in alive}
                stats = subprocess.check_output(['ps', '-axo', 'pid=,pgid=,rss=,%cpu=,state='], text=True)
                for line in stats.splitlines():
                    fields = line.split()
                    if len(fields) == 5 and int(fields[1]) in groups:
                        row['processes'].append({'pid':int(fields[0]), 'pgid':int(fields[1]), 'rss_kib':int(fields[2]), 'cpu':float(fields[3]), 'state':fields[4]})
                if row['pressure'] == 4:
                    critical_since = critical_since or time.monotonic()
                else:
                    critical_since = None
                if not frozen and row['processes'] and (row['pressure'] == 4 or elapsed >= 65):
                    victim = max(row['processes'], key=lambda p:p['rss_kib'])
                    os.killpg(victim['pgid'], signal.SIGSTOP)
                    row['action'] = {'stop':victim['pid'], 'pgid':victim['pgid']}
                    frozen = True
                if not released and (elapsed >= 110 or (critical_since and time.monotonic()-critical_since >= 45)):
                    cleanup(processes)
                    released = True
                    row['action'] = 'cleanup_all'
                log.write(json.dumps(row)+'\n')
                log.flush()
                time.sleep(1)
    finally:
        cleanup(processes)
        for handle in handles:
            handle.close()
        os.kill(watchdog, signal.SIGTERM)
        os.waitpid(watchdog, 0)
    print('Memory probe finished; all owned builds reaped.')

if __name__ == '__main__':
    main()
