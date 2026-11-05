//! bash — the system shell: init starts it on the console. `cd`/`pwd`/
//! `exit`/`echo`/`help`/`kill` are builtins, and anything else is
//! `fork()`+`exec()`+`wait()`ed as an external program from `C:/BIN/`.
//!
//! No variables, globbing, pipes or redirection — just enough to `cd`
//! around, run other ELF binaries with arguments, and see their output
//! and exit status. `fork()` here is a full deep copy of the address
//! space (not copy-on-write — see `basicos_sdk::process`), so it's not
//! free, but it's the only way this kernel spawns a process from inside
//! another one.

#![no_std]
#![no_main]

use basicos_sdk::prelude::*;
use basicos_sdk::process::Fork;

basicos_sdk::entry!(main);

fn main(_args: Args) -> i32 {
    println!();
    println!("basicos — type 'help' for commands. Ctrl-D logs out.");
    loop {
        print_prompt();

        let mut line = String::new();
        match io::read_line(&mut line) {
            Ok(0) => {
                println!(); // echo the newline a real terminal would show on ^D
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("bash: read error: {}", e);
                break;
            }
        }

        let tokens = match tokenize(line.trim_end_matches(['\n', '\r'])) {
            Ok(t) => t,
            Err(msg) => {
                eprintln!("bash: {}", msg);
                continue;
            }
        };
        if let Some(status) = dispatch(&tokens) {
            return status;
        }
    }
    0
}

fn print_prompt() {
    match fs::current_dir() {
        Ok(cwd) => print!("{}> ", cwd),
        Err(_) => print!("bash> "),
    }
}

/// Run one already-tokenized line. `Some(status)` means `exit` was
/// called — the caller should stop the whole shell, not just this line.
fn dispatch(tokens: &[String]) -> Option<i32> {
    let Some(cmd) = tokens.first() else { return None };
    let args: Vec<&str> = tokens.iter().map(String::as_str).collect();
    match cmd.as_str() {
        "cd" => builtin_cd(&args),
        "pwd" => builtin_pwd(),
        "echo" => builtin_echo(&args[1..]),
        "help" => builtin_help(),
        "kill" => builtin_kill(&args),
        "exit" => return Some(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0)),
        _ => run_external(&args),
    }
    None
}

// --- builtins ---------------------------------------------------------------

fn builtin_cd(args: &[&str]) {
    // No `$HOME` here — default to the root of the current drive.
    let target = args.get(1).copied().unwrap_or("C:/");
    if let Err(e) = fs::set_current_dir(target) {
        eprintln!("cd: {}: {}", target, e);
    }
}

fn builtin_pwd() {
    match fs::current_dir() {
        Ok(cwd) => println!("{}", cwd),
        Err(e) => eprintln!("pwd: {}", e),
    }
}

fn builtin_echo(args: &[&str]) {
    println!("{}", args.join(" "));
}

fn builtin_kill(args: &[&str]) {
    let Some(&pid_arg) = args.get(1) else {
        eprintln!("usage: kill <pid> [signal]  (default signal: TERM)");
        return;
    };
    let Ok(pid) = pid_arg.parse::<process::Pid>() else {
        eprintln!("kill: {}: not a pid", pid_arg);
        return;
    };
    let sig = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(process::SIGTERM);
    if let Err(e) = process::kill(pid, sig) {
        eprintln!("kill: {}: {}", pid, e);
    }
}

fn builtin_help() {
    println!("builtins:");
    println!("  cd [path]         change directory (default C:/)");
    println!("  pwd               print the current directory");
    println!("  echo [text...]    print text back");
    println!("  kill <pid> [sig]  send a signal (default TERM; there are no handlers,");
    println!("                    so a fatal signal just ends the process)");
    println!("  exit [code]       exit the shell (default status 0)");
    println!("  help              this message");
    println!("programs in C:/BIN/:");
    println!("  ls cat write rm mv mkdir rmdir   files");
    println!("  ps meminfo uptime drives date    system state");
    println!("  dmesg                            the kernel log");
    println!("  ifconfig arp ping                network");
    println!("  clear                            clear the screen");
    println!("anything else is run as a program: fork, exec, wait for it to exit.");
    println!("a bare name is looked up in the current directory, then C:/BIN/,");
    println!("trying '.ELF' appended if it isn't already there.");
    println!("drives: A: ramdisk, B: initrd (read-only), C: and up: disks. see 'drives'.");
}

// --- running other programs --------------------------------------------------

/// `fork()` + try each of `resolve(args[0])` in turn with `exec()` until
/// one succeeds, + `wait()` for it. The kernel's `execve` folds every
/// failure reason (bad path, bad ELF, out of memory, ...) into a single
/// `ENOENT`, so there's no way to tell "wrong candidate path" apart from
/// "real problem loading it" — every candidate gets tried before giving up.
fn run_external(args: &[&str]) {
    let cwd = fs::current_dir().unwrap_or_else(|_| String::from("C:/"));
    let candidates = candidates(args[0], &cwd);

    match process::fork() {
        Ok(Fork::Child) => {
            for path in &candidates {
                process::exec(path, args);
                // Only returns on failure; try the next candidate.
            }
            eprintln!("bash: {}: command not found", args[0]);
            process::exit(127);
        }
        Ok(Fork::Parent(pid)) => match process::wait(Some(pid)) {
            // The kernel reports a fatal signal as 128 + its number.
            Ok((_, status)) if status > 128 => {
                eprintln!("bash: {}: killed by signal {}", args[0], status - 128)
            }
            Ok(_) => {}
            Err(e) => eprintln!("bash: wait: {}", e),
        },
        Err(e) => eprintln!("bash: fork: {}", e),
    }
}

fn has_extension(path: &str) -> bool {
    path.rsplit('/').next().unwrap_or(path).contains('.')
}

fn join(dir: &str, rel: &str) -> String {
    if dir.ends_with('/') { format!("{}{}", dir, rel) } else { format!("{}/{}", dir, rel) }
}

/// Every path worth trying `exec()` on for the word `word`, typed with
/// `cwd` as the current directory. `execve`'s own path lookup isn't
/// aware of a process's working directory (see `sched::exec_current`),
/// so — unlike `open`/`stat`/`cd` — relative names are resolved here,
/// in userspace, before ever reaching the kernel.
fn candidates(word: &str, cwd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |p: String| {
        if !has_extension(&p) {
            out.push(format!("{}.ELF", p));
        }
        if !out.contains(&p) {
            out.push(p);
        }
    };

    let has_drive = word.len() >= 2 && word.as_bytes()[1] == b':';
    if has_drive || word.starts_with('/') {
        add(word.to_string());
        return out;
    }

    let rel = word.strip_prefix("./").unwrap_or(word);
    add(join(cwd, rel));
    // A bare name (no directory component of its own) also gets looked
    // up in the conventional binaries directory.
    if !rel.contains('/') {
        add(format!("C:/BIN/{}", rel));
    }
    out
}

// --- word splitting -----------------------------------------------------------

/// Split a line into words: whitespace-separated, `'...'` and `"..."`
/// quoting (a backslash inside `"..."` escapes `"` or `\`, nothing
/// else), a bare `\` escapes the next character, and a `#` starting a
/// word runs a comment to the end of the line. No variables, no
/// globbing — there's nothing here that would expand into one.
fn tokenize(line: &str) -> core::result::Result<Vec<String>, &'static str> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut have_token = false;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match c {
            '#' if !have_token => break,
            '\'' => {
                have_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err("unexpected end of line looking for matching '"),
                    }
                }
            }
            '"' => {
                have_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\')) => cur.push(ch),
                            Some(ch) => {
                                cur.push('\\');
                                cur.push(ch);
                            }
                            None => return Err("unexpected end of line looking for matching \""),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err("unexpected end of line looking for matching \""),
                    }
                }
            }
            '\\' => {
                have_token = true;
                match chars.next() {
                    Some(ch) => cur.push(ch),
                    None => return Err("trailing backslash"),
                }
            }
            c if c.is_whitespace() => {
                if have_token {
                    tokens.push(core::mem::take(&mut cur));
                    have_token = false;
                }
            }
            c => {
                have_token = true;
                cur.push(c);
            }
        }
    }
    if have_token {
        tokens.push(cur);
    }
    Ok(tokens)
}
