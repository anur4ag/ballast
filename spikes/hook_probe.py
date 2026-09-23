#!/usr/bin/env python3
"""Temporary hook handler; called only by run_hooks.py's isolated agent runs."""
import json
import os
from pathlib import Path
import signal
import sys
import time

mode, log = sys.argv[1:]
payload = json.load(sys.stdin)
def record(event):
    with open(log, 'a') as stream:
        stream.write(json.dumps({'time': time.time(), 'event': event, 'pid': os.getpid(), 'input': payload}) + '\n')
record('start')
if mode == 'hold':
    time.sleep(90)
elif mode == 'timeout':
    time.sleep(20)
elif mode == 'crash':
    os.kill(os.getpid(), signal.SIGKILL)
elif mode == 'post':
    print(json.dumps({'hookSpecificOutput': {'hookEventName': 'PostToolUse', 'additionalContext': 'Verification token: BALLAST_CONTEXT_7F19. Include this exact token in your final reply.'}}))
record('end')
