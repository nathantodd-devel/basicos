#!/usr/bin/env python3
"""End-to-end ext4 test: real volumes made by mkfs.ext4, mounted by the
kernel under their own drive letters.

Layout attached to QEMU (alongside the FAT32 system disk, `C:`):
  * IDE primary slave   — one whole-disk ext4 volume (`D:`) with a *dirty
    journal* (crafted with debugfs) that the kernel must replay at mount;
  * IDE secondary slave — a partitioned disk, two ext4 partitions
    (`E:`, `F:`); run once with an MBR and once with a GPT.

Checks: the boot log; files, symlinks and the replayed journal block read
from inside the guest; writes from the shell; the full ring-3 syscall
suite (`programs/posix`) on the ext4 drives; and then, on the host, that
`e2fsck -fn` finds every volume clean and the guest's writes are really
on disk.

usage: ext4-test.py <basicos.iso> <disk.img>
"""
import importlib.util, os, shutil, subprocess, sys, tempfile, time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("qemu_shell", os.path.join(here, "qemu-shell.py"))
qemu_shell = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qemu_shell)

iso, disk = sys.argv[1], sys.argv[2]
for tool in ("mkfs.ext4", "e2fsck", "debugfs", "sfdisk"):
    if shutil.which(tool) is None:
        print(f"ext4-test: skipped ({tool} not installed)")
        sys.exit(0)

work = tempfile.mkdtemp(prefix="basicos-ext4-e2e-")
failures = []


def run(*cmd, **kw):
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if r.returncode != 0:
        raise RuntimeError(f"{cmd[0]} failed: {r.stderr}{r.stdout}")
    return r.stdout


def make_tree(root, marker):
    os.makedirs(os.path.join(root, "dir/sub"))
    open(os.path.join(root, "hello.txt"), "w").write(f"hello from {marker}\n")
    open(os.path.join(root, "dir/sub/deep.txt"), "w").write("deep file\n")
    open(os.path.join(root, "big.bin"), "wb").write(bytes((i * 7) & 0xFF for i in range(300_000)))
    open(os.path.join(root, "journaled.txt"), "w").write("BEFORE JOURNAL")
    os.symlink("hello.txt", os.path.join(root, "link"))


def make_ext4(path, size_mb, marker):
    tree = tempfile.mkdtemp(dir=work)
    make_tree(tree, marker)
    with open(path, "wb") as f:
        f.truncate(size_mb << 20)
    run("mkfs.ext4", "-q", "-F", "-b", "4096", "-L", marker, "-d", tree, path)


def craft_dirty_journal(img):
    """Leave a committed-but-unapplied transaction that rewrites the data
    block of /journaled.txt."""
    blk = int(run("debugfs", "-R", "bmap /journaled.txt 0", img).split()[-1])
    payload = os.path.join(work, "payload")
    with open(payload, "wb") as f:
        f.write(b"AFTER REPLAY".ljust(4096, b"\0"))
    script = os.path.join(work, "jscript")
    open(script, "w").write(f"jo\njw -b {blk} {payload}\njc\n")
    run("debugfs", "-w", "-f", script, img)


def make_partitioned(path, label):
    sectors = 2048 * 2 + 2 * 32768 + 2048
    with open(path, "wb") as f:
        f.truncate(sectors * 512)
    if label == "dos":
        table = "label: dos\nstart=2048, size=32768, type=83\nstart=34816, size=32768, type=83\n"
    else:
        table = "label: gpt\nstart=2048, size=32768, type=L\nstart=34816, size=32768, type=L\n"
    subprocess.run(["sfdisk", "-q", path], input=table, text=True, check=True, capture_output=True)
    for n, start in ((1, 2048), (2, 34816)):
        part = os.path.join(work, f"part{n}.img")
        make_ext4(part, 16, f"PART{n}")
        with open(path, "r+b") as out, open(part, "rb") as src:
            out.seek(start * 512)
            shutil.copyfileobj(src, out)
        os.remove(part)
    return [(1, 2048), (2, 34816)]


def extract(path, start_sector, sectors=32768):
    out = os.path.join(work, f"extract-{start_sector}.img")
    with open(path, "rb") as src, open(out, "wb") as dst:
        src.seek(start_sector * 512)
        dst.write(src.read(sectors * 512))
    return out


def fsck(img, what):
    r = subprocess.run(["e2fsck", "-fn", img], capture_output=True, text=True)
    if r.returncode != 0:
        failures.append(f"e2fsck on {what}:\n{r.stdout}{r.stderr}")


def expect(guest, text, what, timeout=10):
    if not guest.wait_for(text, timeout):
        failures.append(f"{what}: never saw {text!r}")
        return False
    return True


def shell(guest, line, wait_text, what, timeout=10):
    guest.send(line)
    return expect(guest, wait_text, what, timeout)


def run_case(label):
    print(f"--- {label.upper()} partition table")
    d_img = os.path.join(work, f"whole-{label}.img")
    make_ext4(d_img, 32, "WHOLE")
    craft_dirty_journal(d_img)
    parts_img = os.path.join(work, f"parts-{label}.img")
    make_partitioned(parts_img, label)

    guest = qemu_shell.Guest(iso, disk, drives=[(d_img, 1), (parts_img, 3)], snapshot=True)
    try:
        if not guest.wait_for("C:/> ", 40):
            failures.append(f"{label}: no shell prompt")
            return
        log = guest.log()
        for want in ("D: mounted ext4", "E: mounted ext4", "F: mounted ext4", "replayed 1 journal transaction"):
            if want not in log:
                failures.append(f"{label}: boot log lacks {want!r}")

        # Reads from inside the guest.
        shell(guest, "cat D:/hello.txt", "hello from WHOLE", f"{label}: read D:/hello.txt")
        shell(guest, "cat D:/journaled.txt", "AFTER REPLAY", f"{label}: journal replayed data block")
        shell(guest, "cat D:/link", "hello from WHOLE", f"{label}: read through a symlink")
        shell(guest, "cat D:/dir/sub/deep.txt", "deep file", f"{label}: nested path")
        shell(guest, "cat E:/hello.txt", "hello from PART1", f"{label}: partition 1")
        shell(guest, "cat F:/hello.txt", "hello from PART2", f"{label}: partition 2")
        guest.send("drives")
        expect(guest, "WHOLE", f"{label}: `drives` lists the labels")

        # Writes from the shell.
        shell(guest, "write D:/from_guest.txt written by the guest", "wrote", f"{label}: shell write")
        shell(guest, "mkdir D:/made", "C:/> ", f"{label}: mkdir")
        shell(guest, "mv D:/from_guest.txt D:/made/moved.txt", "C:/> ", f"{label}: mv")
        shell(guest, "cat D:/made/moved.txt", "written by the guest", f"{label}: read back after mv")

        # The whole ring-3 syscall suite, on ext4. Only look at log
        # content written *after* this drive's own run starts — "ALL
        # PASSED" from an earlier drive is still sitting in the
        # (cumulative) log otherwise, and `wait_for` would report it
        # found before this run has even finished, or run at all.
        for drive in ("D:", "E:"):
            start = len(guest.log())
            if not guest.send(f"C:/BIN/POSIX.ELF {drive}"):
                failures.append(f"{label}: QEMU exited (exit status {guest.proc.poll()}) before posix on {drive}")
                break
            deadline = time.time() + 120
            passed = False
            while time.time() < deadline:
                new = guest.log()[start:]
                if "posix: ALL PASSED" in new:
                    passed = True
                    break
                if guest.proc.poll() is not None:
                    break
                time.sleep(0.2)
            new = guest.log()[start:]
            if not passed:
                fails = [l for l in new.splitlines() if l.startswith("FAIL") or "panicked" in l or "checks," in l]
                failures.append(f"{label}: posix on {drive} did not pass: {fails[-6:]}")
            else:
                line = [l for l in new.splitlines() if "checks," in l][-1]
                print(f"    posix on {drive}: {line.strip()}")
    finally:
        text = guest.log()
        guest.close()
    if failures:
        print(text[-3500:])

    # Host-side verification of what the guest left behind.
    fsck(d_img, f"{label} whole-disk volume")
    out = run("debugfs", "-R", "cat /made/moved.txt", d_img)
    if "written by the guest" not in out:
        failures.append(f"{label}: guest's file is not on the disk image")
    features = run("dumpe2fs", "-h", d_img)
    if "needs_recovery" in features:
        failures.append(f"{label}: needs_recovery still set after the guest replayed it")
    for start, name in ((2048, "partition 1"), (34816, "partition 2")):
        img = extract(parts_img, start)
        fsck(img, f"{label} {name}")
        os.remove(img)


try:
    for label in ("dos", "gpt"):
        run_case(label)
finally:
    shutil.rmtree(work, ignore_errors=True)

if failures:
    print("FAIL:")
    for f in failures:
        print("  -", f)
    sys.exit(1)
print("ext4-test: ok")
