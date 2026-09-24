#!/usr/bin/env python3
"""Measure end-to-end hook process latency against a real observe-mode daemon."""
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time

root = Path(__file__).resolve().parent.parent
binary = root/'target/release/ballast'
with tempfile.TemporaryDirectory(prefix='blt-lat-', dir='/tmp') as temporary:
    home = Path(temporary)
    (home/'config.toml').write_text('mode = "observe"\nnotifications = false\nrecovery_sweep_markers = []\n')
    env = dict(os.environ, BALLAST_HOME=temporary)
    with (home/'daemon.stderr').open('w') as errors:
        daemon = subprocess.Popen([binary,'daemon'],env=env,stdout=subprocess.DEVNULL,stderr=errors)
        try:
            for _ in range(200):
                if (home/'run/ballastd.sock').exists(): break
                time.sleep(.02)
            for agent in ['claude','codex']:
                payload = json.dumps({'session_id':'latency-check','hook_event_name':'PreToolUse','tool_name':'Bash','tool_input':{'command':'printf ok'}})
                samples = []
                for _ in range(55):
                    start = time.perf_counter()
                    result = subprocess.run([binary,'hook',agent],input=payload,text=True,env=env,capture_output=True,timeout=2)
                    elapsed = (time.perf_counter()-start)*1000
                    assert result.returncode == 0 and not result.stdout and not result.stderr
                    samples.append(elapsed)
                samples = sorted(samples[5:])
                print(json.dumps({'agent':agent,'calls':len(samples),'median_ms':statistics.median(samples),'p95_ms':samples[47],'max_ms':max(samples)}),flush=True)
        finally:
            daemon.terminate()
            daemon.wait(timeout=5)
