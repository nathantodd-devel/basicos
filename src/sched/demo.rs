//! Two small proof-of-life tasks for the scheduler.
//!
//! Neither calls [`super::yield_now`] — on purpose. If their output
//! interleaves in the log instead of running fully sequentially
//! (`task-a` then `task-b`), that's the timer IRQ actually preempting
//! a task that never asked to give up the CPU, not the tasks
//! cooperating. Each busy-waits on real elapsed PIT ticks rather than a
//! plain spin count, so the proof doesn't depend on host/QEMU speed.

use crate::arch::interrupts::TIMER_TICKS;
use crate::pr_info;
use core::sync::atomic::Ordering;

const ITERATIONS: u32 = 3;
const TICKS_PER_ITERATION: u64 = 3;

fn busy_wait_ticks(ticks: u64) {
    let start = TIMER_TICKS.load(Ordering::Relaxed);
    while TIMER_TICKS.load(Ordering::Relaxed).wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

fn run(name: &str) {
    for i in 0..ITERATIONS {
        busy_wait_ticks(TICKS_PER_ITERATION);
        pr_info!("[sched] {} iteration {}/{}", name, i + 1, ITERATIONS);
    }
    pr_info!("[sched] {} done", name);
}

pub extern "C" fn demo_task_a() -> ! {
    run("task-a");
    super::exit_current()
}

pub extern "C" fn demo_task_b() -> ! {
    run("task-b");
    super::exit_current()
}
