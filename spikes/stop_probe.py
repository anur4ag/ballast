#!/usr/bin/env python3
"""Self-stopping command, resumed and reaped by run_hooks.py."""
import json
import os
from pathlib import Path
import signal
import sys
import time

p = Path(sys.argv[1])
p.write_text(json.dumps({'pid': os.getpid(), 'start': time.time()}))
print('STOPPING', os.getpid(), flush=True)
os.kill(os.getpid(), signal.SIGSTOP)
print('RESUMED', time.time(), flush=True)
