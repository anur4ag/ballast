#!/usr/bin/env python3
"""Real isolated installer, consent, tmux rendering and terminal-restoration checks.

Run after cargo build --bin ballast. No real agent configs or service labels are used.
"""
import html
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess as sp
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / 'target/debug/ballast'
OUT = ROOT / 'spikes/out/t15-install'
OUT.mkdir(parents=True, exist_ok=True)
HOME = Path(tempfile.mkdtemp(prefix='bl-flow-', dir='/tmp'))
LABEL = f'dev.ballast.flow.{os.getpid()}'
ENV = dict(os.environ, HOME=str(HOME), BALLAST_HOME=str(HOME / '.ballast'),
           CLAUDE_CONFIG_DIR=str(HOME / '.claude'), CODEX_HOME=str(HOME / '.codex'),
           BALLAST_SERVICE_DIR=str(HOME / 'services'), BALLAST_SERVICE_LABEL=LABEL)
TMUX = ['tmux', '-L', f'ballast-install-{os.getpid()}']
for directory in ['.ballast', '.claude', '.codex']:
    (HOME / directory).mkdir()
(HOME / '.ballast/config.toml').write_text('mode = "observe"\nnotifications = false\nrecovery_sweep_markers = []\n')
(HOME / '.claude/settings.json').write_text(json.dumps({'hooks': {'Stop': [{'hooks': [{'type': 'command', 'command': 'true'}]}]}}) + '\n')


def tm(*args):
    return sp.check_output(TMUX + list(args), text=True)


def wait_for(predicate, description):
    for _ in range(200):
        if predicate():
            return
        time.sleep(.05)
    raise AssertionError(description)


def pane(name):
    return tm('capture-pane', '-p', '-t', name)


def start(name, action='install', width=80, theme='dark', no_color=False):
    env_args = [f'{key}={value}' for key, value in ENV.items() if key in ['HOME', 'BALLAST_HOME', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME', 'BALLAST_SERVICE_DIR', 'BALLAST_SERVICE_LABEL']]
    cmd = shlex.join(['env', '-u', 'NO_COLOR'] + env_args + (['NO_COLOR=1'] if no_color else []) + [str(BINARY), action])
    before, after, done = [HOME / f'{name}-{suffix}' for suffix in ['before', 'after', 'done']]
    shell = f'stty -g > {shlex.quote(str(before))}; {cmd}; result=$?; stty -g > {shlex.quote(str(after))}; echo "$result" > {shlex.quote(str(done))}; exec sleep 120'
    tm('new-session', '-d', '-s', name, '-x', str(width), '-y', '55', shell)
    tm('set-option', '-t', name, 'status', 'off')
    style = 'fg=colour235,bg=colour231' if theme == 'light' else 'fg=colour252,bg=colour16'
    tm('set-option', '-t', name, 'window-style', style)
    tm('set-option', '-t', name, 'window-active-style', style)
    wait_for(lambda: 'Continue?' in pane(name), name + ': prompt missing')


def finish(name, key='n', expected=3):
    if key:
        tm('send-keys', '-t', name, key)
    wait_for(lambda: (HOME / f'{name}-done').exists(), name + ': did not exit')
    assert int((HOME / f'{name}-done').read_text()) == expected, pane(name)
    assert (HOME / f'{name}-before').read_text() == (HOME / f'{name}-after').read_text(), name
    assert tm('display-message', '-p', '-t', name, '#{alternate_on}').strip() == '0', name
    tm('kill-session', '-t', name)


def capture(name, theme, width):
    text = pane(name).rstrip() + '\n'
    (OUT / f'{name}.txt').write_text(text)
    (OUT / f'{name}.ansi').write_text(tm('capture-pane', '-e', '-p', '-t', name).rstrip() + '\n')
    lines = text.splitlines()
    fg, bg = ('#252525', '#ffffff') if theme == 'light' else ('#dadada', '#000000')
    spans = ''.join(f'<text x="20" y="{30 + i * 20}">{html.escape(line).replace(chr(32), "&#160;")}</text>' for i, line in enumerate(lines))
    svg = f'<svg xmlns="http://www.w3.org/2000/svg" width="{width * 9 + 40}" height="{len(lines) * 20 + 40}" viewBox="0 0 {width * 9 + 40} {len(lines) * 20 + 40}"><title>Real Ballast {name} terminal capture</title><rect width="{width * 9 + 40}" height="{len(lines) * 20 + 40}" fill="{bg}"/><g fill="{fg}" font-family="Menlo, monospace" font-size="14">{spans}</g></svg>\n'
    (OUT / f'{name}.svg').write_text(svg)


def run(*args, expected=0):
    result = sp.run([str(BINARY), *args], env=ENV, text=True, capture_output=True)
    assert result.returncode == expected, (args, result.returncode, result.stdout, result.stderr)
    return json.loads(result.stdout)


try:
    for width in [60, 80, 120]:
        for theme in ['light', 'dark']:
            name = f'install-{theme}-{width}'
            start(name, width=width, theme=theme)
            capture(name, theme, width)
            assert 'Undo anytime:' in pane(name)
            finish(name)
    assert not (HOME / 'services').exists()
    assert not (HOME / '.codex/hooks.json').exists()

    start('choose')
    tm('send-keys', '-t', 'choose', 'c')
    wait_for(lambda: 'Choose what' in pane('choose'), 'choose missing')
    tm('send-keys', '-t', 'choose', 'Space')
    wait_for(lambda: 'WARNING' in pane('choose'), 'service warning missing')
    (OUT / 'choose-warning.txt').write_text(pane('choose'))
    tm('send-keys', '-t', 'choose', 'Enter')
    wait_for(lambda: 'Continue?' in pane('choose') and 'WARNING' in pane('choose'), 'warning not retained')
    finish('choose')

    start('diff')
    tm('send-keys', '-t', 'diff', 'd')
    wait_for(lambda: '@@' in pane('diff'), 'diff missing')
    beginning = pane('diff')
    tm('send-keys', '-t', 'diff', 'End')
    wait_for(lambda: pane('diff') != beginning, 'diff did not scroll')
    (OUT / 'diff-end.txt').write_text(pane('diff'))
    tm('send-keys', '-t', 'diff', 'Escape')
    wait_for(lambda: 'Continue?' in pane('diff'), 'diff did not return')
    finish('diff')

    start('no-color', no_color=True)
    assert '\x1b[' not in tm('capture-pane', '-e', '-p', '-t', 'no-color')
    finish('no-color', 'Escape')
    for view in ['prompt', 'diff', 'choose']:
        for sig in [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]:
            name = f'{view}-{sig.name}'
            start(name)
            if view != 'prompt':
                tm('send-keys', '-t', name, 'd' if view == 'diff' else 'c')
                wait_for(lambda: tm('display-message', '-p', '-t', name, '#{alternate_on}').strip() == '1', 'alternate screen missing')
            shell_pid = int(tm('display-message', '-p', '-t', name, '#{pane_pid}'))
            rows = sp.check_output(['ps', '-axo', 'pid=,ppid=,comm='], text=True).splitlines()
            children = [int(row.split()[0]) for row in rows if len(row.split()) >= 3 and int(row.split()[1]) == shell_pid and Path(row.split()[2]).name == 'ballast']
            assert len(children) == 1, children
            os.kill(children[0], sig)
            finish(name, key=None)
    start('ctrl-c')
    finish('ctrl-c', 'C-c')

    transcript = ['SCRIPTED CONSENT TRANSCRIPT (not a real coding-agent session)', '$ ballast install --dry-run --json']
    preview = run('install', '--dry-run', '--json')
    transcript += [json.dumps(preview, indent=2), 'Human fixture: I approve this plan.']
    start('apply')
    tm('send-keys', '-t', 'apply', 'c')
    wait_for(lambda: 'Choose what' in pane('apply'), 'apply checklist missing')
    tm('send-keys', '-t', 'apply', 'Down', 'Down', 'Space', 'Enter')
    wait_for(lambda: 'Continue?' in pane('apply'), 'apply checklist did not return')
    tm('send-keys', '-t', 'apply', 'y')
    wait_for(lambda: (HOME / 'apply-done').exists(), 'apply not complete')
    (OUT / 'applied.txt').write_text(pane('apply'))
    assert 'SKIPPED codex hooks' in pane('apply')
    finish('apply', key=None, expected=0)
    assert not (HOME / '.codex/hooks.json').exists(), 'unchecked agent was modified'
    # The preceding human fixture approved both agents; finish that approved plan.
    applied = run('install', '--yes', '--json')
    transcript += ['$ ballast install --yes --json', json.dumps(applied, indent=2)]
    result = run('install', '--yes', '--json', expected=2)
    assert result['status'] == 'no_change'
    assert (HOME / '.codex/hooks.json').exists()
    assert not (HOME / '.codex/config.toml').exists(), 'trust was written'
    transcript += ['$ ballast install --yes --json (repeat after interactive apply)', json.dumps(result, indent=2)]
    path = HOME / '.claude/settings.json'
    path.write_text(path.read_text().replace(str(BINARY), '/tmp/old-ballast'))
    repair = run('install', '--dry-run', '--json')
    assert next(item for item in repair['items'] if item['id'] == 'claude')['changed']
    result = run('install', '--yes', '--json')
    assert result['current_session_needs_restart']
    assert not next(item for item in result['items'] if item['id'] == 'codex')['status'] == 'applied'
    doctor = run('doctor', '--json', expected=1)
    assert next(check for check in doctor['checks'] if check['name'] == 'Codex trust')['status'] == 'action_required'
    transcript += ['$ ballast doctor --json', json.dumps(doctor, indent=2), 'Report: Hooks changed; restart the current agent session. Open Codex and approve the Ballast hooks yourself with /hooks.']
    for width in [60, 80, 120]:
        for theme in ['light', 'dark']:
            name = f'uninstall-{theme}-{width}'
            start(name, action='uninstall', width=width, theme=theme)
            capture(name, theme, width)
            finish(name)
    start('remove', action='uninstall')
    finish('remove', 'Enter', expected=0)
    assert run('uninstall', '--yes', '--json', expected=2)['status'] == 'no_change'
    (OUT / 'agent-transcript.txt').write_text('\n'.join(transcript) + '\n')
    (OUT / 'verification.txt').write_text('PASS: 60/80/120 light/dark install and uninstall; choose service warning; scrollable diff; Enter/y apply; n/Esc/Ctrl-C cancellation; SIGTERM/SIGINT/SIGHUP restore prompt/diff/choose terminal modes; NO_COLOR; real isolated launchd install; no-change; hook-path repair; preserved user hooks; Codex trust left untouched; uninstall.\n')
    print(OUT)
finally:
    sp.run([str(BINARY), 'uninstall', '--yes', '--purge'], env=ENV, capture_output=True)
    sp.run(['launchctl', 'bootout', f'gui/{os.getuid()}/{LABEL}'], capture_output=True)
    sp.run(TMUX + ['kill-server'], capture_output=True)
    import shutil
    shutil.rmtree(HOME)
