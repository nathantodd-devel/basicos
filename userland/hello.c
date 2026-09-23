/*
 * hello.c — the simplest possible program: write a message, exit.
 * Same idea as ../testprog.s, just compiled from C via syscalls.h
 * instead of hand-written as assembly.
 */

#include "syscalls.h"

int main(void) {
    const char msg[] = "Hello from a C program via syscalls.h!\n";
    sys_write(1, msg, sizeof(msg) - 1);
    return 0;
}
