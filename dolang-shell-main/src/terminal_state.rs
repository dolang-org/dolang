use std::io::{IsTerminal, Write, stderr};

use crossterm::{
    cursor::Show,
    event::DisableMouseCapture,
    execute,
    style::ResetColor,
    terminal::{EnableLineWrap, disable_raw_mode},
};

pub(crate) struct TerminalRestoreGuard {
    state: Option<TerminalState>,
}

impl TerminalRestoreGuard {
    pub(crate) fn capture_if_terminal() -> Self {
        let state = stderr()
            .is_terminal()
            .then(TerminalState::capture)
            .flatten();
        Self { state }
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.state.is_some()
    }

    fn restore(&mut self) {
        if let Some(state) = self.state.take() {
            state.restore();
        }
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

struct TerminalState {
    #[cfg(unix)]
    tty: UnixTerminalState,
    #[cfg(windows)]
    tty: WindowsTerminalState,
}

impl TerminalState {
    fn capture() -> Option<Self> {
        Some(Self {
            tty: platform::capture()?,
        })
    }

    fn restore(self) {
        platform::restore(self.tty);
        restore_visual_state();
    }
}

fn restore_visual_state() {
    let _ = disable_raw_mode();
    let mut stderr = stderr();
    let _ = execute!(
        stderr,
        Show,
        DisableMouseCapture,
        EnableLineWrap,
        ResetColor
    );
    let _ = write!(stderr, "\x1b[0m");
}

#[cfg(unix)]
type UnixTerminalState = libc::termios;

#[cfg(windows)]
type WindowsTerminalState = u32;

#[cfg(unix)]
mod platform {
    use std::{io::stderr, mem::MaybeUninit, os::fd::AsRawFd};

    use super::UnixTerminalState;

    pub(super) fn capture() -> Option<UnixTerminalState> {
        let fd = stderr().as_raw_fd();
        let mut termios = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `fd` is a live file descriptor for stderr and `termios` points to valid memory.
        let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
        (rc == 0).then(|| {
            // SAFETY: `tcgetattr` initialized the structure on success.
            unsafe { termios.assume_init() }
        })
    }

    pub(super) fn restore(termios: UnixTerminalState) {
        let fd = stderr().as_raw_fd();
        // SAFETY: `fd` is a live file descriptor for stderr and `termios` is a valid saved snapshot.
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) };
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, SetConsoleMode,
    };

    use super::WindowsTerminalState;

    pub(super) fn capture() -> Option<WindowsTerminalState> {
        // SAFETY: `GetStdHandle` and `GetConsoleMode` are called with the standard stderr handle.
        unsafe {
            let handle = GetStdHandle(STD_ERROR_HANDLE);
            if handle.is_null() {
                return None;
            }
            let mut mode = 0;
            (GetConsoleMode(handle, &mut mode) != 0).then_some(mode)
        }
    }

    pub(super) fn restore(mode: WindowsTerminalState) {
        // SAFETY: restoring the saved console mode to the standard stderr handle is valid.
        unsafe {
            let handle = GetStdHandle(STD_ERROR_HANDLE);
            if !handle.is_null() {
                let _ = SetConsoleMode(handle, mode);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::TerminalRestoreGuard;

    #[test]
    fn capture_if_terminal_is_constructible() {
        let guard = TerminalRestoreGuard::capture_if_terminal();
        let _ = guard.is_active();
    }

    #[test]
    fn drop_is_idempotent_when_inactive() {
        let mut guard = TerminalRestoreGuard { state: None };
        guard.restore();
        guard.restore();
        assert!(!guard.is_active());
    }
}
