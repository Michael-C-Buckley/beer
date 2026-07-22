//! Pseudo-terminal: open a master/slave pair and run the user's shell on it.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Child, Command, ExitStatus, Stdio};

use anyhow::Context;
use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

/// A running child process attached to a PTY master.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    child: Child,
}

impl Pty {
    /// Open a PTY, size it to `cols`x`rows` (with `cell` giving the cell size in
    /// pixels, so the kernel reports a pixel geometry for graphics clients), and
    /// exec the user's login shell on the slave end with `TERM=term`. When `cwd`
    /// is set the child starts there, so a new window inherits its parent's cwd.
    pub fn spawn(
        cols: u16,
        rows: u16,
        cell: (u16, u16),
        term: &str,
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> anyhow::Result<Self> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
            .context("open pty master")?;
        grantpt(&master).context("grantpt")?;
        unlockpt(&master).context("unlockpt")?;
        let slave = ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY)
            .context("open pty slave")?;

        set_winsize(&master, cols, rows, cell)?;

        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        let argv0 = login_argv0(&shell);

        // Hand the slave to the child's stdio. try_clone gives O_CLOEXEC dups, so
        // the parent's copies and the controlling-tty handle vanish at exec.
        let ctty = slave.try_clone().context("dup slave")?;
        let (stdin, stdout, stderr) = (
            slave.try_clone().context("dup slave")?,
            slave.try_clone().context("dup slave")?,
            slave,
        );

        let mut cmd = Command::new(&shell);
        cmd.arg0(&argv0)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // A daemon client forwards its environment; apply it first so the child
        // sees the client's session, then let beer's own invariants win over it.
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.env("TERM", term)
            // Advertise Kitty graphics protocol compatibility. Yazi (and other
            // clients) gate `kgp` vs `kgp_old` on recognising the terminal
            // brand; KITTY_WINDOW_ID is the env check they use for Kitty.
            .env("KITTY_WINDOW_ID", process::id().to_string())
            .env_remove("COLUMNS")
            .env_remove("LINES")
            .env_remove("TERMCAP");

        // SAFETY: setsid and the TIOCSCTTY ioctl are async-signal-safe raw
        // syscalls; ctty is a valid fd captured by move. We touch no parent
        // heap state, satisfying pre_exec's contract.
        unsafe {
            cmd.pre_exec(move || {
                rustix::process::setsid()?;
                rustix::process::ioctl_tiocsctty(&ctty)?;
                Ok(())
            });
        }

        let child = cmd.spawn().context("spawn shell")?;
        Ok(Self { master, child })
    }

    /// The PTY master, for reading child output and writing input.
    pub fn master(&self) -> &OwnedFd {
        &self.master
    }

    /// Inform the kernel (and thus the child) of a new terminal size; `cell` is
    /// the cell size in pixels, carried into the winsize pixel fields.
    pub fn resize(&self, cols: u16, rows: u16, cell: (u16, u16)) -> anyhow::Result<()> {
        set_winsize(&self.master, cols, rows, cell)
    }

    /// Reap the child if it has exited.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

fn set_winsize(master: &OwnedFd, cols: u16, rows: u16, cell: (u16, u16)) -> anyhow::Result<()> {
    let ws = Winsize {
        ws_row: rows,
        ws_col: cols,
        // The pixel geometry lets graphics-protocol clients size images; it is
        // the grid size times the cell size.
        ws_xpixel: cols.saturating_mul(cell.0),
        ws_ypixel: rows.saturating_mul(cell.1),
    };
    tcsetwinsize(master.as_fd(), ws).context("set pty winsize")
}

/// Login-shell argv[0] is the shell's basename with a leading '-'.
fn login_argv0(shell: &OsStr) -> std::ffi::OsString {
    let name = Path::new(shell)
        .file_name()
        .unwrap_or_else(|| OsStr::new("sh"));
    let mut argv0 = std::ffi::OsString::from("-");
    argv0.push(name);
    argv0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv0_is_dash_prefixed_basename() {
        assert_eq!(login_argv0(OsStr::new("/usr/bin/bash")), "-bash");
        assert_eq!(login_argv0(OsStr::new("zsh")), "-zsh");
        assert_eq!(login_argv0(OsStr::new("")), "-sh");
    }
}
