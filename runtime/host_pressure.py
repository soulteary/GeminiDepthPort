"""Bounded /proc-view context, not proof that a particular disk caused a stall."""
from collections import Counter, deque
from pathlib import Path
import time


class HostPressure:
    def __init__(self, proc_root=Path('/proc'), unix_ns=time.time_ns, mono_ns=time.monotonic_ns):
        self.root = Path(proc_root)
        self.unix_ns, self.mono_ns = unix_ns, mono_ns
        self.samples = 0
        self.errors = Counter()
        self.latest = deque(maxlen=8)
        self.largest_io_stalls = []
        self.maximum = {}
        self.last_counters = {}
        self.last_mono = None
        self.counter_resets = Counter()

    def _read(self, name):
        try:
            with (self.root / name).open() as handle:
                value = handle.read(65537)
            if len(value) > 65536:
                raise ValueError('proc size bound')
            return value
        except (OSError, ValueError):
            self.errors[name] += 1
            return None

    def sample(self):
        now, mono = self.unix_ns(), self.mono_ns()
        counters, gauges = {}, {}
        pressure = self._read('pressure/io')
        if pressure is not None:
            try:
                for line in pressure.splitlines():
                    kind, *fields = line.split()
                    if kind not in ('some', 'full'):
                        continue
                    values = dict(field.split('=', 1) for field in fields)
                    total = int(values['total'])
                    if total < 0:
                        raise ValueError('negative counter')
                    counters['io_' + kind + '_stall_us'] = total
            except (ValueError, KeyError):
                self.errors['pressure/io:parse'] += 1
        mem = self._read('meminfo')
        if mem is not None:
            try:
                for line in mem.splitlines():
                    fields = line.split()
                    if fields and fields[0] in ('Dirty:', 'Writeback:', 'MemAvailable:'):
                        if len(fields) != 3 or fields[2] != 'kB' or int(fields[1]) < 0:
                            raise ValueError('meminfo format')
                        gauges[fields[0].rstrip(':').lower() + '_bytes'] = int(fields[1]) * 1024
            except ValueError:
                self.errors['meminfo:parse'] += 1
        for name, wanted, prefix in (
            ('vmstat', {'pgscan_kswapd', 'pgscan_direct', 'pgsteal_kswapd', 'pgsteal_direct', 'oom_kill'}, 'vm_'),
            ('self/io', {'read_bytes', 'write_bytes', 'syscr', 'syscw'}, 'observer_'),
        ):
            content = self._read(name)
            if content is None:
                continue
            try:
                for line in content.splitlines():
                    fields = line.split()
                    key = fields[0].rstrip(':')
                    if key in wanted:
                        number = int(fields[1])
                        if number < 0:
                            raise ValueError('negative counter')
                        counters[prefix + key] = number
            except (ValueError, IndexError):
                self.errors[name + ':parse'] += 1
        deltas = {}
        for key, value in counters.items():
            previous = self.last_counters.get(key)
            if previous is not None:
                if value >= previous:
                    deltas[key] = value - previous
                else:
                    self.counter_resets[key] += 1
            self.last_counters[key] = value
        interval = None if self.last_mono is None else mono - self.last_mono
        if interval is not None and interval < 0:
            self.errors['monotonic-regression'] += 1
            interval = None
        self.last_mono = mono
        for key, value in gauges.items():
            self.maximum[key] = max(self.maximum.get(key, value), value)
        event = {'unix_ns': now, 'monotonic_ns': mono, 'interval_ns': interval,
                 'gauges': gauges, 'counter_deltas': deltas}
        self.latest.append(event)
        # Full is a subset of some; never sum their stall counters.
        stall = deltas.get('io_some_stall_us')
        if stall is not None:
            self.largest_io_stalls.append(event)
            self.largest_io_stalls.sort(key=lambda x: x['counter_deltas']['io_some_stall_us'], reverse=True)
            del self.largest_io_stalls[8:]
        self.samples += 1

    def summary(self):
        return {'scope': 'mounted-proc-view; PSI/vmstat may be system-wide; self/io belongs only to this observer',
                'causation_verified': False, 'samples': self.samples,
                'available': bool(self.last_counters or self.maximum),
                'maximum_gauges': dict(self.maximum), 'latest_intervals': list(self.latest),
                'largest_io_some_intervals': list(self.largest_io_stalls),
                'counter_resets': dict(self.counter_resets), 'unavailable_or_invalid': dict(self.errors),
                'interval_history_capacity': 8}
