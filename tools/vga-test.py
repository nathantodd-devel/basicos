#!/usr/bin/env python3
"""Use basicos as a person at the machine would: press keys on the PS/2
keyboard, and read what's on the VGA screen. usage: vga-test.py ISO DISK"""
import importlib.util, os, re, sys, time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("qemu_shell", os.path.join(here, "qemu-shell.py"))
qemu_shell = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qemu_shell)

iso, disk = sys.argv[1], sys.argv[2]
failures = []
guest = qemu_shell.Guest(iso, disk, snapshot=True, monitor=True)


def wait_screen(want, what, timeout=10):
    """Wait for the regex `want` to appear on screen."""
    end = time.time() + timeout
    while time.time() < end:
        if re.search(want, guest.screen_text()):
            return True
        time.sleep(0.1)
    failures.append(f"{what}: never saw /{want}/ on:\n{guest.screen_text()}")
    return False


try:
    if not guest.wait_for("C:/> ", 30):
        failures.append("no prompt on serial")
    wait_screen(r"C:/>\Z", "prompt on screen")
    time.sleep(0.5)  # let the post-boot log messages arrive

    # After boot, routine kernel messages stay off the screen...
    text = guest.screen_text()
    if "answers ping" in text:
        failures.append("a routine kernel log line landed on the screen")
    if "answers ping" not in guest.log():
        failures.append("the routine kernel log line is missing from serial")
    # ...and the screen is clean: no stray \r, and UTF-8 turned into CP437.
    raw = b"".join(guest.screen())
    if b"\r" in raw:
        failures.append("a carriage return was drawn as a glyph")
    if b"basicos \xc4 type 'help'" not in raw:
        failures.append("the banner's em dash wasn't mapped to CP437")

    # Typing, in both cases, echoed exactly once.
    guest.type("echo Hello World\n")
    wait_screen(r"(?m)^Hello World$", "echo output")
    wait_screen(r"(?m)^C:/> echo Hello World$", "single echo of typing")

    # Backspace, on screen and in the line.
    guest.type("echoo")
    guest.keys(["backspace"])
    guest.type(" fixed\n")
    wait_screen(r"(?m)^fixed$", "backspace edits the line")

    # Backspace across a line wrap.
    guest.type("echo " + "x" * 80)
    guest.keys(["backspace"] * 3)
    guest.type("\n")
    wait_screen(r"(?m)^x{77}$", "backspace across a wrapped line")

    # Keys with no text don't leak escape sequences into the line.
    guest.type("echo a")
    guest.keys(["up", "left", "f1", "delete", "pgup"])
    guest.type("b\n")
    wait_screen(r"(?m)^ab$", "arrows and function keys type nothing")

    # A real command, and its files on another drive.
    guest.type("ls C:/BIN\n")
    wait_screen(r"entries in C:/BIN", "ls")

    # Ctrl-C drops the line.
    guest.type("echo dropped")
    guest.keys(["ctrl-c"])
    guest.type("echo kept\n")
    wait_screen(r"(?m)^kept$", "Ctrl-C")
    if re.search(r"(?m)^dropped$", guest.screen_text()):
        failures.append("Ctrl-C didn't drop the line")

    # Scrollback: put a marker off the top, go back to it, come forward.
    guest.type("echo MARKER\n")
    wait_screen(r"(?m)^MARKER$", "marker")
    for _ in range(3):
        guest.type("help\n")
    wait_screen(r"see 'drives'", "help")
    time.sleep(0.5)
    live = guest.screen_text()
    if "MARKER" in live:
        failures.append("the marker didn't scroll off (the test needs more output)")
    pages = 0
    while pages < 12 and not re.search(r"(?m)^MARKER$", guest.screen_text()):
        guest.keys(["shift-pgup"])
        pages += 1
        time.sleep(0.1)
    wait_screen(r"(?m)^MARKER$", "Shift+PageUp scrolls back", 1)
    guest.keys(["shift-pgdn"] * pages)
    time.sleep(0.2)
    if guest.screen_text() != live:
        failures.append("Shift+PageDown didn't come back to the live screen")
    guest.keys(["shift-pgup"])
    guest.type("echo after\n")
    wait_screen(r"(?m)^after$", "typing returns from scrollback")

    # clear, then Ctrl-D logs out and a new shell starts.
    guest.type("clear\n")
    end = time.time() + 5
    while time.time() < end and guest.screen_text().strip() != "C:/>":
        time.sleep(0.1)
    if guest.screen_text().strip() != "C:/>":
        failures.append(f"clear left: {guest.screen_text()!r}")
    guest.keys(["ctrl-d"])
    wait_screen(r"type 'help' for commands", "logout restarts the shell")

    # A crash reaches the screen too.
    guest.type("C:/TESTPROG/SEGV.ELF\n")
    wait_screen(r"killed by signal 11", "crash report")
    guest.type("echo alive\n")
    wait_screen(r"(?m)^alive$", "still running after a crash")
    if "panicked" in guest.log():
        failures.append("panic in the serial log")
finally:
    try:
        screen = guest.screen_text()
    except Exception as e:
        screen = f"(couldn't read the screen: {e})"
    guest.close()

if failures:
    print("vga-test: FAIL")
    for f in failures:
        print("  ", f)
    print("--- final screen ---")
    print(screen)
    sys.exit(1)
print("vga-test: ok")
