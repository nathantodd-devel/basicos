int main(void) {
  // A big garbage address is unmapped in every address space, so this
  // reliably takes a real #PF via page_fault_handler, and the kernel
  // kills just this process.
  //
  // Has to be *canonical*, though (bits 63:47 all equal bit 47), or
  // the CPU rejects it with #GP before ever attempting a page-table
  // walk -- 0x00007fffdeadb000 sits in the low canonical half
  // (< 0x0000800000000000) and is nowhere near anything ever mapped.
  // `volatile` keeps the store from being optimized away as dead.
  volatile int *t = (volatile int *)0x00007fffdeadb000UL;
  *t = 42;
  return 0;
}
