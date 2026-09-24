import os, tempfile, subprocess as sp, time, pathlib, json
root=pathlib.Path.cwd();out=root/'spikes/out/t09';out.mkdir(parents=True,exist_ok=True)
home=tempfile.mkdtemp(prefix='blt-perf-');pathlib.Path(home,'config.toml').write_text('mode = "observe"\nnotifications = false\nrecovery_sweep_markers = []\n')
env=dict(os.environ,BALLAST_HOME=home)
daemon=sp.Popen([str(root/'target/release/ballast'),'daemon'],env=env,stdout=sp.DEVNULL,stderr=open(out/'daemon.log','w'))
tmux=['tmux','-L','ballast-t09-perf']
def tm(*a):return sp.check_output(tmux+list(a),text=True)
def cpu_rss(pid):
 fields=sp.check_output(['ps','-p',str(pid),'-o','time=,rss='],text=True).split();parts=list(map(float,fields[0].split(':')))
 cpu=sum(v*60**i for i,v in enumerate(reversed(parts)))
 stat=pathlib.Path('/proc')/str(pid)/'stat'
 if stat.exists():
  counters=stat.read_text().rsplit(') ',1)[1].split()
  cpu=(int(counters[11])+int(counters[12]))/os.sysconf('SC_CLK_TCK')
 return cpu,int(fields[1])/1024
try:
 for _ in range(100):
  if pathlib.Path(home,'run/ballastd.sock').exists():break
  time.sleep(.1)
 tm('new-session','-d','-s','perf','-x','80','-y','30',f'env BALLAST_HOME={home} {root}/target/release/ballast top')
 top=int(tm('display-message','-p','-t','perf','#{pane_pid}'))
 time.sleep(3)
 before={name:cpu_rss(pid) for name,pid in [('top',top),('daemon',daemon.pid)]};start=time.monotonic()
 time.sleep(30)
 elapsed=time.monotonic()-start
 after={name:cpu_rss(pid) for name,pid in [('top',top),('daemon',daemon.pid)]}
 result={name:{'cpu_percent_one_core':round(100*(after[name][0]-before[name][0])/elapsed,3),'rss_mib':after[name][1]} for name in before}
 result['seconds']=elapsed
 daemon.terminate();daemon.wait(timeout=5);time.sleep(2)
 capture=tm('capture-pane','-p','-t','perf');assert 'Daemon unreachable' in capture,capture
 (out/'disconnected.txt').write_text(capture)
 start=time.monotonic();tm('send-keys','-t','perf','q')
 for _ in range(100):
  p=sp.run(tmux+['has-session','-t','perf'],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
  if p.returncode:break
  time.sleep(.01)
 result['quit_after_disconnect_ms']=round((time.monotonic()-start)*1000,1)
 (out/'resources.json').write_text(json.dumps(result,indent=2));print(json.dumps(result))
finally:
 if daemon.poll() is None:daemon.terminate();daemon.wait()
 sp.run(tmux+['kill-server'],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
