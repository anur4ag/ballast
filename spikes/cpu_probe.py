#!/usr/bin/env python3
"""30-second CPU-only control using ten tiny workers, then own-group cleanup."""
import json
import signal
import subprocess
import sys
import time
from memory_probe import OUT, cleanup, snapshot

processes = []
def interrupted(signum, frame):
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM, interrupted)
try:
    with (OUT / 'mac-cpu.jsonl').open('w') as log:
        for i in range(40):
            if i == 5:
                processes = [subprocess.Popen([sys.executable, '-c', 'import time; end=time.monotonic()+35\nwhile time.monotonic()<end: pass'], start_new_session=True) for _ in range(10)]
            if i == 35:
                cleanup(processes)
            row = snapshot()
            row['phase'] = 'baseline' if i < 5 else ('cpu' if i < 35 else 'recovery')
            log.write(json.dumps(row)+'\n')
            log.flush()
            time.sleep(1)
finally:
    cleanup(processes)
