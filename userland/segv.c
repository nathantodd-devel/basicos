int main(void) {
  // NULL (0) won't actually fault here: entrypoint.s identity-maps the
  // whole first 1 GiB as present/writable/user-accessible (a bring-up
  // simplification, no real user/kernel memory isolation yet), so page
  // 0 is mapped in every process. A big garbage address is unmapped in
  // every address space instead -- this kernel never maps anything
  // beyond a few MiB total (the shared kernel range plus each
  // process's small private zone, see mem::paging's process-zone
  // docs) -- so this reliably takes a real #PF via page_fault_handler.
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
