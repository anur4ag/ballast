#!/usr/bin/env python3
"""Replay a macOS scenario_native trace and compare pressure-rate windows."""
import argparse
from bisect import bisect_right
from collections import defaultdict
from datetime import datetime, timezone
import json
from pathlib import Path

LEVELS = ['normal', 'elevated', 'critical']


class Hysteresis:
    """PressureState's entry/descent rules; checked against the native trace before use."""
    def __init__(self):
        self.level = 0
        self.higher = None
        self.below = None

    def reset_sample(self):
        self.higher = self.below = None

    def sample(self, now, candidate):
        if candidate > self.level:
            self.below = None
            if self.higher == candidate:
                self.level, self.higher = candidate, None
            else:
                self.higher = candidate
        else:
            self.higher = None
            if candidate < self.level:
                if self.below is None:
                    self.below = now
                if now-self.below >= 10:
                    self.level -= 1
                    self.below = None
            else:
                self.below = None
        return self.level


def candidate(kernel, page, swap):
    if kernel == 4 or (swap is not None and swap > 256):
        return 2
    if kernel == 2 or (page is not None and swap is not None and page+swap > 64):
        return 1
    return 0


def drivers(kernel, page, swap, level):
    reasons = []
    if level == 2:
        if kernel == 4:
            reasons.append('kernel=4')
        if swap is not None and swap > 256:
            reasons.append('swapout>256 MiB/s')
    elif level == 1:
        if kernel == 2:
            reasons.append('kernel=2')
        if page is not None and swap is not None and page+swap > 64:
            reasons.append('pageout+swapout>64 MiB/s')
    return reasons


def summarize(timeline, end):
    transitions, seconds = [], defaultdict(float)
    previous = None
    for index, point in enumerate(timeline):
        duration = (timeline[index+1]['elapsed_s'] if index+1<len(timeline) else end)-point['elapsed_s']
        seconds[point['level']] += max(0, duration)
        if point['level'] != previous:
            entry = dict(point, previous=previous)
            if previous is not None and LEVELS.index(point['level'])>LEVELS.index(previous):
                entry['confirming_pair'] = timeline[max(0,index-1):index+1]
            transitions.append(entry)
            previous = point['level']
    for index, entry in enumerate(transitions):
        entry['lasted_s'] = (transitions[index+1]['elapsed_s'] if index+1<len(transitions) else end)-entry['elapsed_s']
        entry['right_censored'] = index+1 == len(transitions)
    return {'seconds':dict(seconds), 'transitions':transitions, 'timeline':timeline}


def analyze(rows):
    assert rows and all(r['inputs']['kernel_pressure_level'] in (1, 2, 4) for r in rows), 'requires valid macOS kernel inputs'
    times = [r['elapsed_s'] for r in rows]
    assert all(a < b for a, b in zip(times, times[1:])), 'monotonic samples required'
    actual, model, previous_tick = [], Hysteresis(), None
    for row in rows:
        if not row['guardian_tick'] or row['level'] is None:
            continue
        now = row['elapsed_s']
        note = row['note']
        page, swap = note['pageout_mib_per_sec'], note['swapout_mib_per_sec']
        if row['discarded']:
            model.reset_sample()
        else:
            model.sample(now, candidate(row['inputs']['kernel_pressure_level'], page, swap))
        assert LEVELS[model.level] == row['level'], f'native replay mismatch at {now}: {LEVELS[model.level]} != {row["level"]}'
        window = None if previous_tick is None or row['discarded'] else now-previous_tick
        actual.append(dict(elapsed_s=now, time_ms=row['time_ms'], level=row['level'],
                           pageout_mib_s=page, swapout_mib_s=swap, kernel=row['inputs']['kernel_pressure_level'],
                           window_s=window, drivers=drivers(row['inputs']['kernel_pressure_level'], page, swap, model.level)))
        previous_tick = None if row['discarded'] else now
    results = {'native':summarize(actual, times[-1])} if actual else {}

    def rates(index, window, since):
        now = times[index]
        then = max(since, now-window)
        span = now-then
        if span <= 0:
            return None, None, span
        left = min(index, bisect_right(times, then)-1)
        right = min(index, left+1)
        fraction = 0 if left==right else (then-times[left])/(times[right]-times[left])
        current = rows[index]['inputs']
        old = rows[left]['inputs']
        if current['page_size'] != old['page_size']:
            return None, None, span
        output = []
        for counter in ('pageouts', 'swapouts'):
            earlier = old[counter]+fraction*(rows[right]['inputs'][counter]-old[counter])
            change = current[counter]-earlier
            output.append(change*current['page_size']/1048576/span if change>=0 else None)
        return *output, span

    for name, window in [('1s', 1), ('2s', 2), ('5s', 5), ('10s', 10), ('kernel_only', None)]:
        model, timeline, due, last = Hysteresis(), [], times[0], None
        since = None
        for index, row in enumerate(rows):
            now = times[index]
            if now < due:
                continue
            interval = 1 if model.level == 0 else .25
            discarded = last is None or now-last > 5*interval
            if discarded:
                since = None
            elif since is None:
                since = now
            page, swap, span = rates(index, window, since) if window and since is not None else (None, None, None)
            kernel = row['inputs']['kernel_pressure_level']
            if discarded:
                model.reset_sample()
            else:
                model.sample(now, candidate(kernel, page, swap))
            timeline.append(dict(elapsed_s=now, time_ms=row['time_ms'], level=LEVELS[model.level],
                                 pageout_mib_s=page, swapout_mib_s=swap, kernel=kernel, window_s=span,
                                 drivers=drivers(kernel, page, swap, model.level)))
            last = now
            due = now + (1 if model.level == 0 else .25)
        results[name] = summarize(timeline, times[-1])
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    rows = [json.loads(s) for s in args.trace.read_text().splitlines()]
    results = analyze(rows)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output/'calibration.json').write_text(json.dumps(results, indent=2)+'\n')
    text = ['# Pressure calibration', '',
            'Native hysteresis replay exactly matches every recorded Guardian tick.' if 'native' in results else 'Baseline has no native Guardian decisions; all timelines below are modeled from raw inputs.', '',
            '| Model | Normal s | Elevated s | Critical s | Upward transitions |', '| --- | ---: | ---: | ---: | ---: |']
    for name, result in results.items():
        values = result['seconds']
        up = sum(t['previous'] is not None and LEVELS.index(t['level'])>LEVELS.index(t['previous']) for t in result['transitions'])
        text.append(f'| {name} | {values.get("normal",0):.2f} | {values.get("elevated",0):.2f} | {values.get("critical",0):.2f} | {up} |')
    text += ['', '## Native upward transitions', '', '| UTC | From → to | Driver on confirming sample | Rate window s | Pageout MiB/s | Swapout MiB/s | State lasted s |',
             '| --- | --- | --- | ---: | ---: | ---: | ---: |']
    for t in results.get('native', {}).get('transitions', []):
        if t['previous'] is None or LEVELS.index(t['level'])<=LEVELS.index(t['previous']):
            continue
        stamp = datetime.fromtimestamp(t['time_ms']/1000, timezone.utc).isoformat(timespec='milliseconds')
        fmt = lambda v: 'unknown' if v is None else f'{v:.3f}'
        text.append(f'| {stamp} | {t["previous"]} → {t["level"]} | {", ".join(t["drivers"])} | {fmt(t["window_s"])} | {fmt(t["pageout_mib_s"])} | {fmt(t["swapout_mib_s"])} | {t["lasted_s"]:.3f}{" (trace ends)" if t["right_censored"] else ""} |')
    text += ['', 'The counterfactuals retain two matching upward samples and a 10-second descent hold, dropping one level at a time.',
             'Their Guardian cadence is 1 second in Normal and 250 ms otherwise, selecting the first available raw sample at or after each deadline.',
             'Rolling rates use counter differences over exact 1-, 2-, 5- or 10-second windows with linear interpolation between adjacent raw samples.',
             'After startup or a discarded sample, the window uses only the valid prefix until the full window exists.',
             'Kernel-only ignores pageout and swapout rates and retains the same hysteresis.',
             'The table lists every threshold crossed on the confirming sample; JSON retains both consecutive entry samples with their rate windows.',
             'These are retrospective signal-model comparisons, not evidence that enforcement would improve responsiveness or that a particular workload is eligible.']
    (args.output/'calibration.md').write_text('\n'.join(text)+'\n')
    print('\n'.join(text[:10]))


if __name__ == '__main__':
    main()
