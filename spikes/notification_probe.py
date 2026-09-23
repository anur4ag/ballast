#!/usr/bin/env python3
"""Submit one notification from a temporary LaunchAgent, then unload it."""
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import time

out = Path(__file__).resolve().parent / 'out'
label = 'dev.ballast.verification.notification'
domain = f'gui/{os.getuid()}'
with tempfile.TemporaryDirectory(prefix='ballast-notify-') as temporary:
    plist = Path(temporary) / (label + '.plist')
    plist.write_bytes(plistlib.dumps({'Label': label, 'ProgramArguments': ['/usr/bin/osascript', '-e', 'display notification "Bounded verification spike completed its notification call." with title "Ballast verification"'], 'RunAtLoad': True, 'StandardOutPath': str(out / 'notification.stdout'), 'StandardErrorPath': str(out / 'notification.stderr')}))
    result = subprocess.run(['launchctl', 'bootstrap', domain, str(plist)], capture_output=True, text=True)
    evidence = {'bootstrap_exit': result.returncode, 'bootstrap_stderr': result.stderr}
    if result.returncode == 0:
        try:
            time.sleep(3)
            state = subprocess.run(['launchctl', 'print', domain + '/' + label], capture_output=True, text=True)
            evidence['launchctl_print'] = state.stdout
        finally:
            result = subprocess.run(['launchctl', 'bootout', domain + '/' + label], capture_output=True, text=True)
            evidence['bootout_exit'] = result.returncode
    (out / 'notification.json').write_text(json.dumps(evidence, indent=2))
    print(json.dumps(evidence, indent=2))
