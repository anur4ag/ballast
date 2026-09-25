#!/usr/bin/env python3
"""Summarize ticket 17 captures without retaining process identities or commands."""
import argparse
import json
from pathlib import Path

from run_scenarios import percentiles, rows


def pressure_summary(trace):
    totals = dict(cpu_elevated_s=0.0, io_elevated_s=0.0, proposed_or_actual_throttle_s=0.0)
    maxima = {k: None for k in ('cpu_busy_fraction', 'io_busy_fraction', 'agent_cpu_share', 'agent_io_share', 'load_per_core')}
    gates = dict(cpu_busy=0, cpu_load=0, cpu_both=0, cpu_share=0, io_busy=0, io_share=0)
    seen_ticks = set()
    gaps = 0.0
    for i, row in enumerate(trace):
        view = (row.get('note') or {}).get('throttle') or {}
        dt = (trace[i+1]['time_ms']-row['time_ms'])/1000 if i+1 < len(trace) else 0
        if dt > 3:
            gaps += dt
            dt = 0
        assert dt >= 0, 'trace timestamps moved backwards'
        for key in ('cpu', 'io'):
            if view.get(key+'_level') == 'elevated':
                totals[key+'_elevated_s'] += dt
        if view.get('workloads'):
            totals['proposed_or_actual_throttle_s'] += dt
        raw = (row.get('inputs') or {}).get('throttle') or {}
        values = dict(view, load_per_core=raw.get('load_per_core'))
        for key in maxima:
            value = values.get(key)
            if value is not None:
                maxima[key] = max(maxima[key] or 0, value)
        tick = row.get('tick', row['time_ms'])
        if not row.get('guardian_tick', True) or tick in seen_ticks or row.get('discarded'):
            continue
        seen_ticks.add(tick)
        high = lambda key, threshold: values.get(key) is not None and values[key] > threshold
        cpu_busy, cpu_load = high('cpu_busy_fraction', .9), high('load_per_core', 1)
        gates['cpu_busy'] += cpu_busy
        gates['cpu_load'] += cpu_load
        gates['cpu_both'] += cpu_busy and cpu_load
        gates['cpu_share'] += (values.get('agent_cpu_share') or 0) >= .3
        gates['io_busy'] += high('io_busy_fraction', .8)
        gates['io_share'] += (values.get('agent_io_share') or 0) >= .3
    return dict(**totals, maxima=maxima, gate_samples=gates, excluded_gap_s=gaps)


def run_summary(directory):
    result = json.loads((directory/'result.json').read_text())
    plan = result['plan']
    trace = rows(directory/'trace.jsonl')
    decisions = rows(directory/'decisions.jsonl')
    events = rows(directory/'events.jsonl')
    start, end = plan['start']*1000, plan['load_deadline']*1000
    load_trace = [r for r in trace if start <= r['time_ms'] < end]
    throttles = [d for d in decisions if d.get('event') == 'throttle']
    releases = [d for d in decisions if d.get('event') == 'unthrottle']
    first = None if not throttles else (throttles[0]['timestamp_ms']-start)/1000
    last_release = None if not releases else (releases[-1]['timestamp_ms']-end)/1000
    timings = {key: [] for key in ('apply', 'release', 'steady_throttled', 'steady_normal')}
    for row in trace:
        if not row.get('guardian_tick') or row.get('discarded'):
            continue
        elapsed_ms = row['tick_wall_ns'] / 1e6
        events_in_tick = {d['event'] for d in decisions
                          if row['time_ms'] - elapsed_ms - 1 <= d['timestamp_ms'] <= row['time_ms'] + 1}
        active = ((row.get('note') or {}).get('throttle') or {}).get('workloads')
        phase = ('release' if 'unthrottle' in events_in_tick else 'apply' if 'throttle' in events_in_tick
                 else 'steady_throttled' if active else 'steady_normal')
        timings[phase].append(elapsed_ms)
    return dict(
        scenario=result['scenario'], enforced=result['enforced'],
        outcome=result['outcome'], emergency=result['emergency'],
        probes=result['resource_probes'],
        first_throttle_s=first, first_throttle_is_proposal=not result['enforced'],
        last_release_relative_load_end_s=last_release,
        pressure=pressure_summary(load_trace),
        workers_started=len(result['workers_started']),
        workers_completed=sum(e['event']=='worker_done' for e in events),
        lint_tasks_completed=sum(e.get('tasks', 0) for e in events if e['event']=='lint_completed'),
        disk_load_bytes=sum(e['bytes'] for e in events if e['event']=='disk_written'),
        total_data_write_bytes=result['bulk_write_bytes'],
        remaining_throttles_before_cleanup=result['remaining_throttles_before_cleanup'],
        cleanup_verified=result['cleanup_verified'], temp_removed=result['temp_removed'],
        post_release_seconds=result['post_release_seconds'],
        collector_tick_wall_ns=result['collector_tick_wall_ns'],
        timing_by_action_ms={key: dict(samples=len(values), percentiles=percentiles(values))
                             for key, values in timings.items()},
        load_samples=sum(s.get('phase')=='load' for s in rows(directory/'probe.jsonl')),
        tail_samples=sum(s.get('phase')=='tail' for s in rows(directory/'probe.jsonl')),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('capture', type=Path)
    parser.add_argument('--ambient', type=Path, help='normalized trace with time_ms, inputs, note, tick and discarded fields')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    runs = [run_summary(p.parent) for p in sorted(args.capture.glob('*/result.json'))]
    data = dict(runs=runs, total_data_write_bytes=sum(r['total_data_write_bytes'] for r in runs))
    if args.ambient:
        trace = rows(args.ambient)
        data['ambient'] = pressure_summary(trace)
        data['ambient']['observed_s'] = (trace[-1]['time_ms']-trace[0]['time_ms'])/1000 if trace else 0
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output/'summary.json').write_text(json.dumps(data, indent=2)+'\n')
    table = ['| Scenario | Mode | Phase | CPU-work p99 ms | Write/fsync p99 ms | Random-read p99 ms |',
             '| --- | --- | --- | ---: | ---: | ---: |']
    for r in runs:
        for phase, probes in r['probes'].items():
            values = ['n/a' if not probes[k] else f'{probes[k]["p99"]:.3f}'
                      for k in ('cpu_work_ms', 'write_fsync_ms', 'random_read_ms')]
            table.append(f'| {r["scenario"]} | {"enforced" if r["enforced"] else "baseline"} | {phase} | '+ ' | '.join(values)+' |')
    (args.output/'probes.md').write_text('\n'.join(table)+'\n')


if __name__ == '__main__':
    main()
