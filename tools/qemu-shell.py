"""Shared helpers for the QEMU-driven tests (net-test.py, posix-test.py):
boot basicos headless and talk to its shell over the serial port."""
import subprocess, tempfile, time


class Guest:
    """A headless QEMU guest. `send()` types a line into its shell;
    `log()` is everything it has printed on serial so far."""

    def __init__(self, iso, disk, hostfwd_udp=(), drives=(), snapshot=False):
        """`drives`: extra `(image_path, ide_index)` disks (index 1 is the
        primary slave, 3 the secondary slave — 2 is the CD-ROM).
        `snapshot`: throw away writes to the main disk `disk`."""
        self._log = tempfile.TemporaryFile()
        netdev = "user,id=n0" + "".join(f",hostfwd=udp::{h}-:{g}" for h, g in hostfwd_udp)
        cmd = ["qemu-system-x86_64", "-cdrom", iso, "-m", "256M", "-serial", "stdio",
               "-monitor", "none", "-display", "none", "-boot", "d",
               "-drive", f"file={disk},format=raw,if=ide,index=0" + (",snapshot=on" if snapshot else "")]
        for path, index in drives:
            cmd += ["-drive", f"file={path},format=raw,if=ide,index={index}"]
        cmd += ["-netdev", netdev, "-device", "rtl8139,netdev=n0"]
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=self._log, stderr=subprocess.STDOUT)

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
