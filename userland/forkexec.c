/*
 * forkexec.c — brk/mmap/munmap, then fork + execve + wait4. C version of
 * ../forktest.s's workout, minus the raw-asm bookkeeping the kernel's
 * non-standard syscall ABI needs (see syscalls.h's header comment) —
 * that's exactly what having a real C toolchain buys you.
 *
 * The child execve()s into C:/hello.elf — the *assembly*-built
 * ../testprog.s binary the top-level Makefile already copies onto the
 * test disk — on purpose, to prove a C-compiled process and a
 * hand-assembled one interoperate through the same syscall ABI.
 */

#include "syscalls.h"

static void print(const char *s) {
    size_t len = 0;
    while (s[len]) {
        len++;
    }
    sys_write(1, s, len);
}

int main(void) {
    /* brk: grow the heap, prove the new page is real, independent memory. */
    void *brk0 = sys_brk(NULL);
    sys_brk((char *)brk0 + 8192);
    volatile unsigned long *heap_word = (unsigned long *)brk0;
    *heap_word = 0x1eadbeef;
    if (*heap_word != 0x1eadbeef) {
        print("forkexec: brk FAILED\n");
        return 1;
    }

    /* mmap: anonymous page, prove it's real (and independent) memory too. */
    void *m = sys_mmap(NULL, 4096, PROT_WRITE, MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) {
        print("forkexec: mmap FAILED\n");
        return 1;
    }
    volatile unsigned long *mmap_word = (unsigned long *)m;
    *mmap_word = 0x4afebabe;
    if (*mmap_word != 0x4afebabe) {
        print("forkexec: mmap verify FAILED\n");
        return 1;
    }
    sys_munmap(m, 4096);

    print("forkexec: brk/mmap/munmap ok\n");

    long pid = sys_fork();
    if (pid == 0) {
        print("forkexec: child execve-ing into C:/hello.elf\n");
        char *const argv[] = {"C:/hello.elf", NULL};
        sys_execve("C:/hello.elf", argv, NULL);
        /* only reached if execve failed */
        print("forkexec: execve failed\n");
        sys_exit(1);
    }
    if (pid < 0) {
        print("forkexec: fork FAILED\n");
        return 1;
    }

    print("forkexec: parent forked a child, waiting\n");
    int status = 0;
    sys_wait4(pid, &status, 0);
    print("forkexec: parent reaped child\n");

    return 0;
}
