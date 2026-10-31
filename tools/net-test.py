#!/usr/bin/env python3
"""End-to-end network test: boot basicos headless in QEMU and talk to it.

Checks, against the real RTL8139 driver and QEMU's user-mode NAT:
  * outbound: the kernel's boot self-test resolves (ARP) and pings the
    gateway (10.0.2.2) — looked for in the serial log;
  * inbound, kernel service: UDP datagrams of assorted sizes, forwarded
    from a host port to the in-kernel echo service (guest port 7), come
    back byte-for-byte — which needs ARP, IPv4 and UDP receive *and*
    transmit to work, with checksums the host's stack accepts;
  * inbound, userspace: the same through the socket syscalls — the
    shell starts programs/udpecho (guest port 8000) and the datagrams
    round-trip through a ring-3 process.

usage: net-test.py <basicos.iso> <disk.img> [seconds-of-echo-traffic]
"""
import importlib.util, os, socket, sys, time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("qemu_shell", os.path.join(here, "qemu-shell.py"))
qemu_shell = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qemu_shell)

iso, disk = sys.argv[1], sys.argv[2]
duration = float(sys.argv[3]) if len(sys.argv) > 3 else 5.0
KERNEL_PORT = 15007  # host side of the forward to guest:7
USER_PORT = 15008    # ... to guest:8000

guest = qemu_shell.Guest(iso, disk, hostfwd_udp=[(KERNEL_PORT, 7), (USER_PORT, 8000)])
failures = []


def wait_up(port, seconds):
    """Probe until the guest echoes on `port`; then drain stray late echoes."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(0.5)
    deadline = time.time() + seconds
    while time.time() < deadline:
        sock.sendto(b"ready?", ("127.0.0.1", port))
        try:
            if sock.recvfrom(64)[0] == b"ready?":
                time.sleep(1)
                sock.settimeout(0.05)
                try:
                    while True:
                        sock.recvfrom(4096)
                except socket.timeout:
                    pass
                sock.settimeout(0.5)
                return sock
        except socket.timeout:
            pass
    return None


def soak(sock, port, label, seconds):
    sizes = [0, 1, 17, 100, 512, 1000, 1400, 1472]  # 1472 = max for MTU 1500
    sent = bad = 0
    end = time.time() + seconds
    while time.time() < end:
        for size in sizes:
            msg = os.urandom(size)
            sock.sendto(msg, ("127.0.0.1", port))
            sent += 1
            try:
                if sock.recvfrom(4096)[0] != msg:
                    bad += 1
            except socket.timeout:
                bad += 1
    print(f"{label}: {sent} datagrams, {bad} lost or corrupted")
    if bad:
        failures.append(f"{label}: {bad}/{sent} echoes failed")


try:
    sock = wait_up(KERNEL_PORT, 30)
    if sock is None:
        failures.append("guest never answered a UDP echo on the kernel service within 30 s")
    else:
        soak(sock, KERNEL_PORT, "kernel udp echo", duration)

        # Userspace: start the echo server from the shell.
        guest.send("C:/BIN/UDPECHO.ELF")
        if not guest.wait_for("udpecho: listening", 15):
            failures.append("udpecho never started (no 'listening' line)")
        else:
            usock = wait_up(USER_PORT, 15)
            if usock is None:
                failures.append("userspace udpecho never answered")
            else:
                soak(usock, USER_PORT, "userspace udp echo", duration)
                usock.sendto(b"quit", ("127.0.0.1", USER_PORT))
                if not guest.wait_for("udpecho: quit after", 10):
                    failures.append("udpecho did not exit on 'quit'")

    if "answers ping" not in guest.log():
        failures.append("no 'gateway ... answers ping' line in the boot log")
finally:
    log = guest.log()
    guest.close()

if failures:
    print("FAIL:", "; ".join(failures))
    print("--- serial log ---")
    print(log)
    sys.exit(1)
print("net-test: ok")
