/*
 * fileio.c — exercises open/write/read/close (lseek too), the syscalls
 * hello.c and forkexec.c don't touch. Writes a file to the writable
 * ramdisk (A:), reads it back, and prints what it got.
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
    print("fileio: writing A:/ctest.txt\n");
    long fd = sys_open("A:/ctest.txt", O_WRONLY | O_CREAT | O_TRUNC, 0);
    if (fd < 0) {
        print("fileio: open (write) failed\n");
        return 1;
    }
    const char msg[] = "written from a C program\n";
    sys_write(fd, msg, sizeof(msg) - 1);
    sys_close(fd);

    print("fileio: reading it back\n");
    fd = sys_open("A:/ctest.txt", O_RDONLY, 0);
    if (fd < 0) {
        print("fileio: open (read) failed\n");
        return 1;
    }
    char buf[64];
    long n = sys_read(fd, buf, sizeof(buf) - 1);
    sys_close(fd);
    if (n < 0) {
        print("fileio: read failed\n");
        return 1;
    }
    buf[n] = '\0';
    print(buf);

    return 0;
}
