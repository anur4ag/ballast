#!/usr/bin/env python3
"""Paired same-host daemon ticks; only spawned observe daemons are terminated."""
import json, os, pathlib, socket, statistics, subprocess as sp, sys, tempfile, time
baseline=pathlib.Path(sys.argv[1]).resolve()
candidate=pathlib.Path(sys.argv[2]).resolve()
out=pathlib.Path(sys.argv[3]);out.parent.mkdir(parents=True,exist_ok=True)
def resources(pid):
    fields=sp.check_output(['ps','-p',str(pid),'-o','time=,rss='],text=True).split()
    cpu=sum(float(v)*60**i for i,v in enumerate(reversed(fields[0].split(':'))))
    stat=pathlib.Path('/proc')/str(pid)/'stat'
    if stat.exists():
        counters=stat.read_text().rsplit(') ',1)[1].split()
        cpu=(int(counters[11])+int(counters[12]))/os.sysconf('SC_CLK_TCK')
    return cpu,int(fields[1])/1024
runs=[]
for name,binary in [('baseline',baseline),('candidate',candidate),('candidate',candidate),('baseline',baseline)]:
    with tempfile.TemporaryDirectory(prefix='blt-t12-perf-') as home:
        home=pathlib.Path(home)
        (home/'config.toml').write_text('mode = "observe"\nnotifications = false\nrecovery_sweep_markers = []\n')
        daemon=sp.Popen([str(binary),'daemon'],env=dict(os.environ,BALLAST_HOME=str(home)),stdout=sp.DEVNULL,stderr=sp.DEVNULL)
        try:
            until=time.monotonic()+10
            while not (home/'run/ballastd.sock').exists():
                assert daemon.poll() is None
                assert time.monotonic()<until
                time.sleep(.05)
            connection=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
            connection.settimeout(5);connection.connect(str(home/'run/ballastd.sock'))
            stream=connection.makefile('rwb',buffering=0)
            rows=[];last=0;before=None
            while len(rows)<24:
                stream.write(b'{"version":1,"method":"status"}\n')
                status=json.loads(stream.readline())['status']
                if status['tick']!=last:
                    last=status['tick']
                    if last>3:
                        if before is None:before=resources(daemon.pid);start=time.monotonic()
                        rows.append({k:status[k] for k in ['tick_cpu_ns','tick_wall_ns','process_count','pressure_level']})
                time.sleep(.1)
            after=resources(daemon.pid);seconds=time.monotonic()-start
            result={'name':name,'ticks':rows,'cpu_percent_one_core':100*(after[0]-before[0])/seconds,'rss_mib':after[1]}
            runs.append(result)
            print(name,'complete',flush=True)
            stream.close();connection.close()
        finally:
            daemon.terminate();daemon.wait(timeout=5)
summary={}
for name in ['baseline','candidate']:
    rows=[t for r in runs if r['name']==name for t in r['ticks']]
    def pct(key,p):return sorted(t[key] for t in rows)[int((len(rows)-1)*p)]/1e6
    summary[name]={'ticks':len(rows),'cpu_median_ms':pct('tick_cpu_ns',.5),'cpu_p95_ms':pct('tick_cpu_ns',.95),'wall_median_ms':pct('tick_wall_ns',.5),'wall_p95_ms':pct('tick_wall_ns',.95),'daemon_cpu_percent':statistics.mean(r['cpu_percent_one_core'] for r in runs if r['name']==name),'rss_mib':statistics.mean(r['rss_mib'] for r in runs if r['name']==name)}
summary['cpu_median_delta_ms']=summary['candidate']['cpu_median_ms']-summary['baseline']['cpu_median_ms']
summary['cpu_median_delta_percent']=100*summary['cpu_median_delta_ms']/summary['baseline']['cpu_median_ms']
out.write_text(json.dumps({'summary':summary,'runs':runs},indent=2)+'\n')
print(json.dumps(summary,indent=2))
