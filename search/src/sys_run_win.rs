//! sys_run.rs on Windows: the part of it the rest of fewer uses. Windows has no
//! pseudo-terminals here, no sessions and no signals: `run` reads pipes there (run.rs), and a
//! background process is started in a process group of its own, with no window.

use std::process::Command;

pub const SIGKILL: i32 = 9;

/// Start `c` away from this program's console: in a process group of its own (Ctrl-C there
/// does not reach it) and with a hidden console of its own (closing the window does not end
/// it, and the programs it starts open no window).
pub fn detach(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    c.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Windows has no signals: false, nothing sent (a program is ended with Child::kill).
pub fn send_signal(_pid: i32, _sig: i32) -> bool {
    false
}

/// `run` never gives a command a pseudo-terminal on Windows: as if stdout were no terminal.
pub fn is_tty(_fd: i32) -> bool {
    false
}

/// Ctrl-C and Ctrl-Break reach the command, which shares the console, and not `run`: it goes
/// on reading the command's output and records its end.
pub fn leave_ctrl_c_to_the_command() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    // CTRL_C_EVENT (0) and CTRL_BREAK_EVENT (1) are taken; closing the window, logging off
    // and shutting down are not
    unsafe extern "system" fn taken(kind: u32) -> i32 {
        (kind <= 1) as i32
    }
    // SAFETY: adds a handler that only returns a number
    unsafe {
        SetConsoleCtrlHandler(Some(taken), 1);
    }
}
