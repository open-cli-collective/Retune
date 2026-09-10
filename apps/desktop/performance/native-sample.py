"""Read three 5-second CPU/footprint samples for explicitly attributed macOS PIDs.

Usage: python3 native-sample.py /tmp/native-process output.json PID [PID ...]
CPU 100% means one core. Audio is controlled separately by the native fixture.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

probe, output, *pids = sys.argv[1:]
assert pids and all(pid.isdecimal() and int(pid) > 0 for pid in pids)
def read(pid):
    return json.loads(subprocess.check_output([probe, str(pid)], text=True))

# Small runnable checks for the probe's input boundary and counter units.
assert subprocess.run([probe, '1x'], capture_output=True).returncode == 2
own = read(os.getpid())
assert own['physicalFootprintBytes'] > 0 and own['cpuNs'] > 0
samples = []
for _ in range(3):
    before = [read(pid) for pid in pids]
    start = time.monotonic_ns()
    time.sleep(5)
    after = [read(pid) for pid in pids]
    elapsed = time.monotonic_ns() - start
    samples.append(dict(durationMs=elapsed / 1e6, before=before, after=after,
                        cpuPercent=sum(b['cpuNs'] - a['cpuNs'] for a, b in zip(before, after)) / elapsed * 100,
                        summedPhysicalFootprintBytes=sum(p['physicalFootprintBytes'] for p in after)))
Path(output).write_text(json.dumps(samples, indent=2) + '\n')
print(json.dumps([dict(cpuPercent=s['cpuPercent'], footprintMiB=s['summedPhysicalFootprintBytes'] / 1048576) for s in samples]))
