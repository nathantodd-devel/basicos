"""Shared helpers for the QEMU-driven tests (net-test.py, posix-test.py):
boot basicos headless and talk to its shell over the serial port."""
import os, socket, subprocess, tempfile, time


class Guest:
    """A headless QEMU guest. `send()` types a line into its shell;
    `log()` is everything it has printed on serial so far."""

    def __init__(self, iso, disk, hostfwd_udp=(), drives=(), snapshot=False, monitor=False):
        """`drives`: extra `(image_path, ide_index)` disks (index 1 is the
        primary slave, 3 the secondary slave — 2 is the CD-ROM).
        `snapshot`: throw away writes to the main disk `disk`.
        `monitor`: open QEMU's monitor, for `monitor_cmd`."""
        self._log = tempfile.TemporaryFile()
        netdev = "user,id=n0" + "".join(f",hostfwd=udp::{h}-:{g}" for h, g in hostfwd_udp)
        self._dir = tempfile.TemporaryDirectory()
        self._sock = os.path.join(self._dir.name, "monitor")
        mon = f"unix:{self._sock},server,nowait" if monitor else "none"
        cmd = ["qemu-system-x86_64", "-cdrom", iso, "-m", "256M", "-serial", "stdio",
               "-monitor", mon, "-display", "none", "-boot", "d",
               "-drive", f"file={disk},format=raw,if=ide,index=0" + (",snapshot=on" if snapshot else "")]
        for path, index in drives:
            cmd += ["-drive", f"file={path},format=raw,if=ide,index={index}"]
        cmd += ["-netdev", netdev, "-device", "rtl8139,netdev=n0"]
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=self._log, stderr=subprocess.STDOUT)

    def monitor_cmd(self, line):
        """Run one QEMU monitor command."""
        end = time.time() + 10
        while not os.path.exists(self._sock) and time.time() < end:
            time.sleep(0.05)
        if not hasattr(self, "_mon"):
            self._mon = socket.socket(socket.AF_UNIX)
            self._mon.connect(self._sock)
            self._mon_read()
        self._mon.sendall((line + "\n").encode())
        reply = self._mon_read()
        if b"Try \"help" in reply:
            raise RuntimeError(f"monitor refused {line!r}: {reply[-200:]!r}")

    def _mon_read(self):
        """Read up to the prompt after a command has run. (The monitor also
        redraws its prompt as it echoes input, hence the newline check.)"""
        data = b""
        while b"\n" not in data or not data.endswith(b"(qemu) "):
            data += self._mon.recv(4096)
        return data

    def keys(self, names):
        """Press keys on the PS/2 keyboard, by QEMU key name."""
        for name in names:
            self.monitor_cmd(f"sendkey {name}")
            time.sleep(0.03)

    def type(self, text):
        """Type `text` on the PS/2 keyboard."""
        names = {" ": "spc", "\n": "ret", "/": "slash", ":": "shift-semicolon", ".": "dot", "-": "minus"}
        self.keys(names.get(c, f"shift-{c.lower()}" if c.isupper() else c) for c in text)

    def screen(self):
        """What's on the VGA text screen: a list of 25 rows of raw bytes."""
        path = os.path.join(self._dir.name, "vga")
        self.monitor_cmd(f'pmemsave 0xb8000 4000 "{path}"')
        with open(path, "rb") as f:
            cells = f.read()
        chars = cells[0::2]
        return [chars[i:i + 80] for i in range(0, 2000, 80)]

    def screen_text(self):
        return "\n".join(r.decode("latin-1").rstrip() for r in self.screen())

    def log(self):
        self._log.seek(0)
        return self._log.read().decode(errors="replace")

    def send(self, line):
        """Type a line into the shell; False if the guest has gone away."""
        try:
            self.proc.stdin.write((line + "\n").encode())
            self.proc.stdin.flush()
            return True
        except BrokenPipeError:
            return False

    def wait_for(self, text, timeout):
        """Block until `text` shows up in the log; True if it did."""
        end = time.time() + timeout
        while time.time() < end:
            if text in self.log():
                return True
            if self.proc.poll() is not None:
                return text in self.log()
            time.sleep(0.1)
        return False

    def close(self):
        self.proc.kill()
        self.proc.wait()
