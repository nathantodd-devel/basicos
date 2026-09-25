//! forkwait — fork, then have the child exec `hello` with arguments and
//! the parent wait for it. Covers the whole process API, and (through
//! hello's output) that `exec` passes argv correctly.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;
use process::Fork;

basicos_sdk::entry!(main);

fn main(_args: Args) -> Result<()> {
    match process::fork()? {
        Fork::Child => {
            println!("forkwait: child (pid {}) exec'ing hello", process::getpid());
            let err = process::exec("C:/BIN/HELLO.ELF", &["hello", "exec'd", "from", "forkwait"]);
            eprintln!("forkwait: exec failed: {}", err);
            process::exit(1);
        }
        Fork::Parent(child) => {
            println!("forkwait: parent forked pid {}, waiting", child);
            let (pid, status) = process::wait(Some(child))?;
            println!("forkwait: reaped pid {} with status {}", pid, status);
            // No children left: wait must now say so.
            match process::wait(None) {
                Err(Errno::ECHILD) => println!("forkwait: no more children (ECHILD), as expected"),
                other => println!("forkwait: UNEXPECTED second wait: {:?}", other),
            }
        }
    }
    Ok(())
}
