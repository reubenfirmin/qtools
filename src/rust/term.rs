use std::fs::{File, OpenOptions};
use std::io::{stdout, IsTerminal};
use std::os::unix::io::AsRawFd;

/**
 * Column width of the terminal attached to stdout, or None if stdout isn't a terminal.
 *
 * Done with a direct TIOCGWINSZ ioctl so we don't pull in a crate just for this. The whole tool is
 * already Linux-only (it reads st_dev and skips /proc, /sys), so the hard-coded request is fine.
 */
pub fn stdout_width() -> Option<usize> {
    let out = stdout();
    if !out.is_terminal() {
        return None;
    }

    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }

    const TIOCGWINSZ: u64 = 0x5413;

    let mut ws = Winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let ret = unsafe { ioctl(out.as_raw_fd(), TIOCGWINSZ, &mut ws as *mut Winsize) };

    if ret == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

/**
 * A snapshot of the controlling terminal's line mode, held so it can be restored.
 */
pub struct TtyMode {
    file: File,
    termios: libc::termios
}

/**
 * Snapshot the controlling terminal's mode via /dev/tty, so cooked mode can be restored if a
 * capability probe leaves the terminal in raw mode after timing out.
 */
pub fn save_tty_mode() -> Option<TtyMode> {
    let file = OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    let ok = unsafe { libc::tcgetattr(file.as_raw_fd(), &mut termios) == 0 };
    if ok {
        Some(TtyMode { file, termios })
    } else {
        None
    }
}

pub fn restore_tty_mode(mode: &TtyMode) {
    unsafe {
        libc::tcsetattr(mode.file.as_raw_fd(), libc::TCSANOW, &mode.termios);
    }
}

/// Discard any pending terminal input on the controlling terminal. Used to throw away leftover
/// capability-query replies (e.g. a DA1 `ESC[?...c`) that a probe didn't consume, before they echo
/// onto the screen once the terminal is back in cooked/echo mode.
pub fn drain_input(mode: &TtyMode) {
    unsafe {
        libc::tcflush(mode.file.as_raw_fd(), libc::TCIFLUSH);
    }
}

/// Turn off echo and canonical mode on the terminal (keeping every other flag from `mode`). Held for
/// the whole capability-probe window so a query reply that arrives cannot echo onto the screen; the
/// original mode is put back with `restore_tty_mode` afterwards.
pub fn set_noecho(mode: &TtyMode) {
    let mut raw = mode.termios;
    raw.c_lflag &= !(libc::ECHO | libc::ICANON);
    unsafe {
        libc::tcsetattr(mode.file.as_raw_fd(), libc::TCSANOW, &raw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::FromRawFd;
    use std::process::{Command, Stdio};

    #[test]
    fn stdout_geometry_matches_redirected_and_terminal_output() {
        const EXPECTED_WIDTH: &str = "QTOOLS_TEST_STDOUT_WIDTH";
        if let Ok(expected) = std::env::var(EXPECTED_WIDTH) {
            assert_eq!(stdout_width(), expected.parse::<usize>().ok());
            return;
        }

        // Use child processes so each case gets its own stdout without changing the test runner's.
        let check = |output: Stdio, expected: &str| {
            let result = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "term::tests::stdout_geometry_matches_redirected_and_terminal_output"])
                .env(EXPECTED_WIDTH, expected)
                .stdout(output)
                .output()
                .unwrap();
            assert!(result.status.success(), "stdout width {expected}: {}{}",
                String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        };

        check(Stdio::piped(), "none");
        for width in [126, 0] {
            let mut master = -1;
            let mut slave = -1;
            let size = libc::winsize { ws_row: 24, ws_col: width, ws_xpixel: 0, ws_ypixel: 0 };
            assert_eq!(unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size)
            }, 0, "{}", std::io::Error::last_os_error());
            let _master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            check(slave.into(), if width == 0 { "none" } else { "126" });
        }
    }

    #[test]
    fn tty_snapshot_is_optional_and_restorable_when_present() {
        if let Some(mode) = save_tty_mode() {
            set_noecho(&mode);
            drain_input(&mode);
            restore_tty_mode(&mode);
        }
    }
}
