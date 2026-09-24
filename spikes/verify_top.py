#!/usr/bin/env python3
"""Run after cargo build --release --example top_fixture --bin ballast."""
import os, tempfile, subprocess as sp, time, json, pathlib, shlex, signal, termios
root=pathlib.Path.cwd()
out=root/'spikes/out/t09';out.mkdir(parents=True,exist_ok=True)
home=tempfile.mkdtemp(prefix='blt-top-')
pathlib.Path(home,'config.toml').write_text('notifications = false\nrecovery_sweep_markers = []\n')
env=dict(os.environ,BALLAST_HOME=home)
server=sp.Popen([str(root/'target/release/examples/top_fixture')],env=env,stdout=sp.DEVNULL,stderr=open(out/'fixture.log','w'))
tmux=['tmux','-L','ballast-t09']
def tm(*a): return sp.check_output(tmux+list(a),text=True)
def ps(): return json.loads(sp.check_output([str(root/'target/release/ballast'),'ps','--json'],env=env))['snapshot']
try:
 for _ in range(100):
  if pathlib.Path(home,'run/ballastd.sock').exists(): break
  time.sleep(.1)
 pathlib.Path(home,'pressure').write_text('critical')
 for _ in range(100):
  s=ps()
  if s['frozen']: break
  time.sleep(.1)
 assert len(s['frozen'])==1
 # Use the production hook bridge; queued command is a synthetic harmless display label.
 payload={'session_id':'agent-b','hook_event_name':'PreToolUse','tool_name':'Bash','tool_input':{'command':'cargo test --synthetic-fixture'}}
 hook=sp.Popen([str(root/'target/release/ballast'),'hook','claude'],env=env,stdin=sp.PIPE,stdout=sp.PIPE,stderr=sp.PIPE,text=True)
 hook.stdin.write(json.dumps(payload));hook.stdin.close()
 for _ in range(60):
  s=ps()
  if s['held']: break
  time.sleep(.1)
 assert len(s['held'])==1,s['status']
 # Every pane runs exactly the executable built in this worktree.
 for width in (80,120,160):
  for theme,fg,bg in [('dark','colour252','colour16'),('light','colour235','colour231')]:
   name=f'{theme}-{width}'
   tm('new-session','-d','-s',name,'-x',str(width),'-y','32',shlex.join(['env', '-u', 'NO_COLOR', f'BALLAST_HOME={home}', str(root/'target/release/ballast'), 'top']))
   tm('set-option','-t',name,'status','off')
   tm('set-option','-t',name,'window-style',f'fg={fg},bg={bg}')
   tm('set-option','-t',name,'window-active-style',f'fg={fg},bg={bg}')
   time.sleep(2)
   capture=tm('capture-pane','-p','-t',name).rstrip()+'\n'
   (out/f'{name}.txt').write_text(capture)
   (out/f'{name}.ansi').write_text(tm('capture-pane','-e','-p','-t',name).rstrip()+'\n')
   assert 'FROZEN' in capture and 'HELD' in capture,capture
   tm('send-keys','-t',name,'q')
 # External signals must leave the terminal canonical and on its main screen.
 for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
  name=f'signal-{sig.name}'
  before=pathlib.Path(home,f'{name}-before');after=pathlib.Path(home,f'{name}-after')
  done=pathlib.Path(home,f'{name}-done')
  command=f'stty -g > {shlex.quote(str(before))}; {shlex.join(["env", f"BALLAST_HOME={home}", str(root/"target/release/ballast"), "top"])}; stty -g > {shlex.quote(str(after))}; touch {shlex.quote(str(done))}; exec sleep 30'
  tm('new-session','-d','-s',name,'-x','80','-y','24',command)
  pane=int(tm('display-message','-p','-t',name,'#{pane_pid}'))
  for _ in range(100):
   children=sp.check_output(['ps','-axo','pid=,ppid=,comm='],text=True).splitlines()
   owned=[int(row.split()[0]) for row in children if len(row.split())>=3 and int(row.split()[1])==pane and pathlib.Path(row.split()[2]).name=='ballast']
   if owned and tm('display-message','-p','-t',name,'#{alternate_on}').strip()=='1':break
   time.sleep(.05)
  assert len(owned)==1
  os.kill(owned[0],sig)
  for _ in range(100):
   if done.exists():break
   time.sleep(.05)
  assert done.exists(),f'{sig.name}: did not exit'
  assert before.read_text()==after.read_text(),f'{sig.name}: terminal mode changed'
  assert tm('display-message','-p','-t',name,'#{alternate_on}').strip()=='0'
  tty=tm('display-message','-p','-t',name,'#{pane_tty}').strip()
  with open(tty,'rb',buffering=0) as stream:
   assert termios.tcgetattr(stream)[3] & termios.ICANON
  tm('kill-session','-t',name)
 (out/'signals.txt').write_text('SIGTERM, SIGINT, SIGHUP: original terminal modes restored, ICANON set, alternate_on=0\n')
 pathlib.Path(home,'pressure').write_text('normal')
 for _ in range(400):
  s=ps()
  if not s['held'] and not s['frozen']: break
  time.sleep(.1)
 assert not s['held'] and not s['frozen']
 hook.wait(timeout=2)
 # Fixture exits normally and thaws before dropping owned children.
 print(json.dumps({'home':home,'fixture_pid':server.pid,'captures':str(out),'resumed':True,'admitted':True}))
finally:
 sp.run(tmux+['kill-server'],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
 # Signal only fixture-owned PIDs. Resume first while its IPC is still alive.
 sp.run([str(root/'target/release/ballast'),'resume','--all'],env=env,stdout=sp.DEVNULL,stderr=sp.DEVNULL)
 # Tell the fixture to finish gracefully via its pressure input.
 pathlib.Path(home,'pressure').write_text('exit')
 try: server.wait(timeout=3)
 except sp.TimeoutExpired:
  # The bounded fixture exits after 150 seconds and owns all child cleanup.
  server.wait(timeout=155)
