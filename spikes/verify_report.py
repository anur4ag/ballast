#!/usr/bin/env python3
"""Bounded owned-workload report check and terminal captures. Build release first."""
import json, os, pathlib, shlex, socket, subprocess as sp, tempfile, time
root = pathlib.Path.cwd()
out = root / 'spikes/out/t12'
out.mkdir(parents=True, exist_ok=True)
home = pathlib.Path(tempfile.mkdtemp(prefix='ballast-report-live-'))
(home/'config.toml').write_text('notifications = false\nrecovery_sweep_markers = []\n')
env = dict(os.environ, BALLAST_HOME=str(home))
binary = root/'target/release/ballast'
server = sp.Popen([str(root/'target/release/examples/top_fixture')], env=env, stdout=sp.DEVNULL, stderr=open(out/'fixture.log', 'w'))
tmux = ['tmux', '-L', f'ballast-t12-{os.getpid()}']
def tm(*args): return sp.check_output(tmux+list(args), text=True)
def cli(*args): return sp.check_output([str(binary), *args], env=env, text=True)
def snapshot(): return json.loads(cli('ps','--json'))['snapshot']
def until(predicate, seconds=40):
    end = time.monotonic()+seconds
    while time.monotonic() < end:
        result = predicate()
        if result: return result
        time.sleep(.1)
    raise AssertionError('fixture did not reach expected state')
def captures(prefix):
    for width in (80,160):
        for theme,fg,bg in [('dark','colour252','colour16'),('light','colour235','colour231')]:
            name=f'{prefix}-{theme}-{width}'
            tm('new-session','-d','-s',name,'-x',str(width),'-y','32',shlex.join(['env','-u','NO_COLOR',f'BALLAST_HOME={home}',str(binary),'top']))
            tm('set-option','-t',name,'status','off')
            tm('set-option','-t',name,'window-style',f'fg={fg},bg={bg}')
            tm('set-option','-t',name,'window-active-style',f'fg={fg},bg={bg}')
            time.sleep(1.2)
            pane=tm('capture-pane','-p','-t',name).rstrip()+'\n'
            assert 'today:' in pane and 'freeze' in pane and 'hold' in pane,pane
            # Synthetic command labels are not copied into durable evidence.
            for suffix,content in [('txt',pane),('ansi',tm('capture-pane','-e','-p','-t',name).rstrip()+'\n')]:
                (out/f'{name}.{suffix}').write_text(content.replace('cargo test --synthetic-fixture','[held command omitted]'))
            tm('send-keys','-t',name,'q')
hook = None
try:
    until(lambda:(home/'run/ballastd.sock').exists(),10)
    (home/'pressure').write_text('critical')
    until(lambda:len(snapshot()['frozen'])==1,10)
    payload={'session_id':'agent-b','hook_event_name':'PreToolUse','tool_name':'Bash','tool_input':{'command':'cargo test --synthetic-fixture'}}
    hook=sp.Popen([str(binary),'hook','claude'],env=env,stdin=sp.PIPE,stdout=sp.PIPE,stderr=sp.PIPE,text=True)
    hook.stdin.write(json.dumps(payload));hook.stdin.close()
    until(lambda:len(snapshot()['held'])==1,10)
    captures('active')
    # Wait for an observed post-freeze comparison, then let admission and thaw complete.
    def comparison_ready():
        r=json.loads(cli('report','--since','1d','--json'))
        f=r['totals']['enforce']['freezes_by_agent_kind'].get('claude',{})
        return any(not pair.endswith('unknown') for pair in f.get('pressure_after_30s',{}))
    until(comparison_ready,40)
    (home/'pressure').write_text('normal')
    until(lambda:not snapshot()['held'] and not snapshot()['frozen'])
    hook.wait(timeout=3)
    until(lambda:json.loads(cli('report','--json'))['hold_waits']['enforce']['completed']==1,5)
    captures('completed')
    live=json.loads(cli('report','--since','1d','--json'))
    events=[json.loads(line) for line in (home/'log/decisions.jsonl').read_text().splitlines()]
    t=live['totals']['enforce']
    assert sum(f['count'] for f in t['freezes_by_agent_kind'].values())==sum(e['event']=='freeze' for e in events)
    assert t['holds']==sum(e['event']=='hold' for e in events)==1
    waits=[e['details']['wait_ms']//1000 for e in events if e['event']=='hold_completed']
    assert live['hold_waits']['enforce']['median_seconds']==waits[0]
    assert t['elevated_ms']>0 and t['critical_ms']>0
    assert t['freezes_by_agent_kind']['claude']['total_ms']>0
    (out/'report.txt').write_text(cli('report','--since','1d'))
    (out/'report.json').write_text(json.dumps(live,indent=2)+'\n')
    (home/'pressure').write_text('exit');server.wait(timeout=5)
    offline=json.loads(cli('report','--since','1d','--json'))
    assert offline['totals']['enforce']['holds']==live['totals']['enforce']['holds']
    assert offline['hold_waits']==live['hold_waits']
    (out/'live-check.json').write_text(json.dumps({'freezes':sum(f['count'] for f in t['freezes_by_agent_kind'].values()),'holds':1,'wait_seconds':waits[0],'comparison':t['freezes_by_agent_kind']['claude']['pressure_after_30s'],'offline_counts_match':True},indent=2)+'\n')
    print((out/'live-check.json').read_text())
finally:
    sp.run(tmux+['kill-server'],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
    if server.poll() is None:
        (home/'pressure').write_text('exit');server.wait(timeout=5)
    if hook is not None and hook.poll() is None:
        hook.terminate();hook.wait(timeout=3)
