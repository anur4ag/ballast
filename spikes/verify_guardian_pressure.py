#!/usr/bin/env python3
"""Native-pressure guardian check: four owned workers, 8 GiB total cap, 150s deadline."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading

root = Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--out', type=Path, required=True)
out = parser.parse_args().out.resolve()
out.mkdir(parents=True, exist_ok=True)
subprocess.run(['cargo', 'build', '--release', '--example', 'guardian_pressure'], cwd=root, check=True)
subprocess.run(['cc', '-O2', str(root/'examples/guardian_memory.c'), '-o', str(out/'worker')], check=True)
with tempfile.TemporaryDirectory(prefix='blt-pressure-') as home, (out/'stderr.log').open('w') as errors:
    groups = []
    child = subprocess.Popen([str(root/'target/release/examples/guardian_pressure'), str(out/'worker')],
        env={'PATH': '/usr/bin:/bin', 'BALLAST_HOME': home, 'BALLAST_OWNER': 'guardian-pressure-probe'},
        stdout=subprocess.PIPE, stderr=errors, text=True, start_new_session=True)

    def cleanup():
        for group in groups:
            for sig in (signal.SIGCONT, signal.SIGTERM):
                try:
                    os.killpg(group, sig)
                except ProcessLookupError:
                    pass
        if child.poll() is None:
            child.terminate()

    timer = threading.Timer(150, cleanup)
    timer.start()
    try:
        with (out/'pressure.jsonl').open('w') as log:
            for line in child.stdout:
                log.write(line)
                log.flush()
                row = json.loads(line)
                groups.extend(row.get('owned_worker_groups', []))
                if row.get('summary'):
                    print(json.dumps(row), flush=True)
        code = child.wait(timeout=5)
    finally:
        cleanup()
        timer.cancel()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
        for group in groups:
            try:
                os.killpg(group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        import shutil
        shutil.copytree(Path(home)/'log', out/'log', dirs_exist_ok=True)
    raise SystemExit(code)
