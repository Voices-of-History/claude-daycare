"""Run a check command and fail if any observed descendant survives it.

Uses ps on Linux/macOS. Does not inspect arguments or environments. Sampling
is evidence for the tested CLI version, not proof about future subprocesses.
"""
import json
import os
import pathlib
import signal
import subprocess
import sys
import time


def processes():
    rows = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,comm='], text=True)
    return {int(pid): (int(parent), name) for pid, parent, name in
            (line.strip().split(None, 2) for line in rows.splitlines())}


report = pathlib.Path(sys.argv[1])
child = subprocess.Popen(sys.argv[2:])
seen = {}
try:
    while child.poll() is None:
        table = processes()
        family = {child.pid} | set(seen)
        while True:
            found = {pid for pid, (parent, _) in table.items() if parent in family}
            if found <= family:
                break
            family |= found
        seen.update({pid: table[pid] for pid in family if pid != child.pid and pid in table})
        time.sleep(.025)
    # Allow reaping at exit, then inspect known descendants even if reparented.
    time.sleep(.2)
    alive = sorted(set(seen) & set(processes()))
    report.write_text(json.dumps({'observed': seen, 'survivors': alive}, indent=2))
    assert not alive, f'runner left processes alive: {alive}'
    assert any('opencode' in name for _, name in seen.values()), 'no OpenCode process observed'
    sys.exit(child.returncode)
finally:
    if child.poll() is None:
        child.terminate()
        child.wait(timeout=10)
    for pid in set(seen) & set(processes()):
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
