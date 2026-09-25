#!/usr/bin/env python3
"""Summarize a throwaway phase-instrumented scenario 8 capture; never generates load."""
import argparse
import json
from pathlib import Path

from run_scenarios import percentiles, rows

INTERVALS = {
    'Launch to audit constructor': ('launch', 'loaded'),
    'Audit constructor to hook entry': ('loaded', 'hook_run_entry'),
    'Hook entry to stdin done (includes worker startup)': ('hook_run_entry', 'stdin_done'),
    'Stdin done to parsed request': ('stdin_done', 'request_parsed'),
    'Parsed request to connected socket': ('request_parsed', 'connect_done'),
    'Connect done to request sent': ('connect_done', 'request_sent'),
    'Request sent to hook response (overlaps daemon phases)': ('request_sent', 'hook_response_received'),
    'Hook entry to response': ('hook_run_entry', 'hook_response_received'),
    'Hook entry to main-thread decision': ('hook_run_entry', 'hook_main_received'),
    'IPC request to snapshot-lock attempt': ('ipc_request_received', 'worker_snapshot_wait'),
    'IPC snapshot-lock acquisition': ('worker_snapshot_wait', 'worker_snapshot_acquired'),
    'IPC snapshot acquired to all lookups done': ('worker_snapshot_acquired', 'lookups_done'),
    'Ancestry walk': ('ancestry_begin', 'ancestry_done'),
    'Port check (no port target in these paths)': ('ports_begin', 'ports_done'),
    'Argument lookup': ('argv_begin', 'argv_done'),
    'Queue submission to tick handling': ('queue_submit', 'tick_request_received'),
    'Tick snapshot-lock acquisition': ('tick_snapshot_wait', 'tick_snapshot_acquired'),
    'Snapshot target matching': ('target_match_begin', 'target_match_done'),
    'Target match done to worker decision ready': ('target_match_done', 'ipc_decision_ready'),
    'Worker decision ready to socket write done': ('ipc_decision_ready', 'ipc_decision_sent'),
}


def analyze(directory):
    result = {}
    for path in ('deny_pid', 'deny_argv', 'hold'):
        summary = json.loads((directory/'hooks'/f'{path}.json').read_text())
        calls = []
        for sample in summary['samples']:
            audit = rows(directory/'hooks'/f'{path}-{sample["index"]}.jsonl')
            loaded = next(e for e in audit if e['event']=='loaded')
            events = rows(directory/'phases'/f'{loaded["pid"]}.jsonl')
            assert events, f'no phases for {path} call {sample["index"]}'
            times = {'loaded': loaded['ns']}
            for event in events:
                times.setdefault(event['event'], event['ns'])
            reply = next((e for e in audit if e['event']=='first_response'), None)
            if reply and sample['spawn_to_response_ms'] is not None:
                times['launch'] = reply['ns'] - round(sample['spawn_to_response_ms']*1e6)
            intervals = {}
            for name, (start, end) in INTERVALS.items():
                if start in times and end in times:
                    assert times[end]>=times[start], (path, name, times)
                    intervals[name] = (times[end]-times[start])/1e6
            internal = intervals.get('Hook entry to main-thread decision')
            calls.append(dict(index=sample['index'], pid=loaded['pid'], verdict=sample['verdict'],
                              intervals_ms=intervals, headroom_ms=None if internal is None else 200-internal,
                              timeline_ms={k:(v-times['hook_run_entry'])/1e6 for k,v in sorted(times.items(),key=lambda kv:kv[1])}))
        phase_stats = {}
        for name in INTERVALS:
            values = [c['intervals_ms'][name] for c in calls if name in c['intervals_ms']]
            phase_stats[name] = dict(n=len(values), ms=percentiles(values))
        measured = [c for c in calls if c['headroom_ms'] is not None]
        result[path] = dict(phase_stats=phase_stats, calls=calls, eligible_calls=summary['eligible_calls'],
                            fail_open_rate=summary['fail_open_rate'],
                            min_headroom_ms=min((c['headroom_ms'] for c in measured),default=None),
                            worst_call=min(measured,key=lambda c:c['headroom_ms']) if measured else None)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    result = analyze(args.directory)
    args.output.mkdir(parents=True,exist_ok=True)
    (args.output/'phases.json').write_text(json.dumps(result,indent=2)+'\n')
    text = ['# Hook phase timings', '', 'All intervals use monotonic timestamps.',
            'Hook and daemon phases overlap; rows must not be summed.',
            'Each path has ten calls, so p99 is the observed maximum.', '']
    for path,data in result.items():
        text += [f'## {path}', '', f"Minimum measured internal headroom: {data['min_headroom_ms']} ms.", '',
                 '| Phase | N | p50 ms | p99 ms | max ms |', '| --- | --- | --- | --- | --- |']
        for name,stats in data['phase_stats'].items():
            q=stats['ms']
            values=' | '.join(f'{q[k]:.3f}' for k in ('p50','p99','max')) if q else 'not exercised | not exercised | not exercised'
            text.append(f"| {name} | {stats['n']} | {values} |")
        worst=data['worst_call']
        if worst:
            text += ['', f"Worst internal call: index {worst['index']}, PID {worst['pid']}, verdict {worst['verdict']}.", '',
                     '| Event | ms relative to hook entry |', '| --- | --- |']
            text += [f'| {name} | {value:.3f} |' for name,value in worst['timeline_ms'].items()]
        text.append('')
    (args.output/'phases.md').write_text('\n'.join(text)+'\n')
    print(json.dumps({k:{'min_headroom_ms':v['min_headroom_ms'],'fail_open_rate':v['fail_open_rate']} for k,v in result.items()}))


if __name__ == '__main__':
    main()
