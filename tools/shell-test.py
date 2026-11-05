#!/usr/bin/env python3
"""Boot basicos headless and drive the userspace shell: every command works,
a crashing or malicious program takes down only itself, repeated fork/exec
is solid, and logging out brings a new shell. usage: shell-test.py ISO DISK"""
import importlib.util, os, re, sys

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("qemu_shell", os.path.join(here, "qemu-shell.py"))
qemu_shell = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qemu_shell)

iso, disk = sys.argv[1], sys.argv[2]
failures = []
guest = qemu_shell.Guest(iso, disk, snapshot=True)


def run(line, want, timeout=15):
    """Type a command, and check for `want` (a regex) in what it printed."""
    start = len(guest.log())
    guest.send(line)
    text = ""
    import time
    end = time.time() + timeout
    while time.time() < end:
        text = guest.log()[start:]
        if re.search(want, text):
            return True
        time.sleep(0.05)
    failures.append(f"{line!r}: never saw /{want}/ in {text[-300:]!r}")
    return False


try:
    if not guest.wait_for("C:/> ", 30):
        failures.append("shell prompt never appeared")
    run("help", "ifconfig arp ping")
    run("ps", r"\n\s+1\s+0\s+\w+\s+INIT\.ELF")
    run("ps", r"\[net\]")
    run("dmesg", "INFO: basicos booted!")
    run("dmesg", r"(?m)^\[ *\d+\.\d+\] INFO: Features: heap paging .*\bnet\r?$")
    run("meminfo", r"frames: \d+/\d+ free")
    run("uptime", r"uptime: \d+\.\d+s")
    run("drives", r"C:\s+fat32")
    run("date", r"\d{4}-\d\d-\d\d \d\d:\d\d:\d\d UTC")
    run("ls C:/BIN", r"HELLO\.ELF|hello\.elf")
    run("cat C:/hello.txt", "hello from the FAT32 test disk")
    run("cat C:/nope.txt", "cat: C:/nope.txt:")
    run("write A:/one.txt written from userspace", "wrote 22 bytes")
    run("mkdir A:/dir", "A:/> |C:/> ")
    run("mv A:/one.txt A:/dir/two.txt", "C:/> ")
    run("cat A:/dir/two.txt", "written from userspace")
    run("rm A:/dir/two.txt", "C:/> ")
    run("rmdir A:/dir", "C:/> ")
    run("ls A:/", r"\(0 entries")
    run("ifconfig", r"eth0: 10\.0\.2\.15")
    run("ping 10.0.2.2 2", "2 sent, 2 received")
    run("ping 127.0.0.1 1", "1 sent, 1 received")
    run("ping 10.0.2.99 1", "1 sent, 0 received")
    run("arp", r"10\.0\.2\.2\s")
    run("clear", "C:/> ")
    run("nosuchprogram", "command not found")

    # A crash costs only the program that crashed.
    run("C:/TESTPROG/SEGV.ELF", "killed by signal 11")
    run("echo still here", "still here")

    # Isolation, failed exec, orphan reparenting.
    run("isotest", "isotest: (ALL PASSED|FAILED)", 30)
    log = guest.log()
    if "isotest: ALL PASSED" not in log:
        failures.append("isotest did not pass")
    if "orphan adopted by init" not in log:
        failures.append("orphan was not adopted by init")
    if not re.search(r"init: reaped orphan \d+", log):
        run("echo", "C:/> ")
        if not re.search(r"init: reaped orphan \d+", guest.log()):
            failures.append("init did not reap the orphan")

    # The fork/exec race used to hit one boot in two.
    for i in range(25):
        if not run("forkwait", r"no more children \(ECHILD\)", 10):
            break

    # Logging out starts a fresh shell.
    run("exit", "type 'help' for commands", 10)
    run("echo back again", "back again")

    log = guest.log()
    for bad in ("EXCEPTION", "panicked", "double fault"):
        if bad in log:
            failures.append(f"{bad!r} in the serial log")
finally:
    log = guest.log()
    guest.close()

if failures:
    print("shell-test: FAIL")
    for f in failures:
        print("  ", f)
    print("--- end of serial log ---")
    print(log[-1500:])
    sys.exit(1)
print("shell-test: ok")
