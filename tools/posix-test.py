#!/usr/bin/env python3
"""Boot basicos headless and run the programs/posix self-test through
the shell. Passes if the program reports `ALL PASSED`; on failure prints
its FAIL lines (or the whole serial log if it never finished).

usage: posix-test.py <basicos.iso> <disk.img>
"""
import importlib.util, os, sys

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("qemu_shell", os.path.join(here, "qemu-shell.py"))
qemu_shell = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qemu_shell)

iso, disk = sys.argv[1], sys.argv[2]
guest = qemu_shell.Guest(iso, disk)
try:
    if not guest.wait_for("C:/> ", 30):
        print("FAIL: shell prompt never appeared")
    guest.send("C:/BIN/POSIX.ELF")
    done = guest.wait_for("posix: ALL PASSED", 60) or guest.wait_for(" failed", 1)
    log = guest.log()
finally:
    guest.close()

if "posix: ALL PASSED" in log:
    line = next(l for l in log.splitlines() if "checks," in l)
    print(line.strip())
    print("posix-test: ok")
    sys.exit(0)
print("posix-test: FAIL")
for l in log.splitlines():
    if l.startswith("FAIL") or "panicked" in l or "EXCEPTION" in l or "checks," in l:
        print("  ", l)
if not done:
    print("--- serial log ---")
    print(log)
sys.exit(1)
