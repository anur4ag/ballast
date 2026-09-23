#!/usr/bin/env python3
"""Small executable check of the recorded conclusions, not a product test suite."""
import json
from pathlib import Path
import statistics

out = Path(__file__).resolve().parent / 'out'
def rows(name):
    return [json.loads(line) for line in (out / name).read_text().splitlines()]
def rate(previous, current, key='swapouts'):
    return (current[key]-previous[key])*current['page_size']/(current['time']-previous['time'])/1048576

assert rate({'time':1,'swapouts':0},{'time':3,'swapouts':128,'page_size':16384}) == 1
for agent in ('claude','codex'):
    hook = rows(agent+'-hold-hook.jsonl')
    assert hook[1]['time']-hook[0]['time'] >= 89
    assert 'BALLAST_COMMAND_DONE' in (out/(agent+'-hold.jsonl')).read_text()
for mode in ('timeout','crash'):
    assert [r['event'] for r in rows('codex-'+mode+'-hook.jsonl')] == ['start']
    assert any(r.get('item',{}).get('exit_code') == 0 for r in rows('codex-'+mode+'.jsonl'))
assert 'BALLAST_CONTEXT_7F19' in rows('codex-post.jsonl')[-2]['item']['text']
assert 'RESUMED' in (out/'codex-stop.jsonl').read_text()
for filename in ('mac-memory.jsonl','mac-cpu.jsonl'):
    data = rows(filename)
    assert all(b['time'] > a['time'] for a,b in zip(data,data[1:]))
    for phase in dict.fromkeys(r['phase'] for r in data):
        samples = [r for r in data if r['phase']==phase]
        rates = [rate(a,b) for a,b in zip(data,data[1:]) if b['phase']==phase]
        latency = sorted(r['launch_ms'] for r in samples)
        print(filename,phase,json.dumps({'samples':len(samples),'pressure':sorted(set(r['pressure'] for r in samples)), 'max_swapout_mib_s':round(max(rates or [0]),2),'launch_median_ms':round(statistics.median(latency),2),'launch_p95_ms':round(latency[int((len(latency)-1)*.95)],2),'launch_max_ms':round(max(latency),2)}))
data = rows('mac-memory.jsonl')
freeze = next(r for r in data if isinstance(r.get('action'),dict))
normal = next(r for r in data if r['elapsed']>freeze['elapsed'] and r['pressure']==1)
warn = next(r for r in data if r['elapsed']>normal['elapsed'] and r['pressure']==2)
print('Freeze to normal seconds:',normal['elapsed']-freeze['elapsed'])
print('Normal to warning seconds:',warn['elapsed']-normal['elapsed'])
print('Recorded evidence checks passed.')
