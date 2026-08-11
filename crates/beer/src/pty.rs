//! Pseudo-terminal: open a master/slave pair and run the user's shell on it.

use std::{
  ffi::{OsStr, OsString},
  io,
  os::{
    fd::{AsFd, OwnedFd},
    unix::process::CommandExt,
  },
  path::Path,
  process::{self, Child, Command, ExitStatus, Stdio},
};

use anyhow::Context;
use rustix::{
  pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt},
  termios::{Winsize, tcsetwinsize},
};

/// A running child process attached to a PTY master.
#[derive(Debug)]
pub struct Pty {
  master: OwnedFd,
  child:  Child,
}

#[expect(
  clippy::absolute_paths,
  reason = "PTY setup names platform process and filesystem types explicitly"
)]
impl Pty {
  /// Open a PTY, size it to `cols`x`rows` (with `cell` giving the cell size in
  /// pixels, so the kernel reports a pixel geometry for graphics clients), and
  /// exec the user's login shell on the slave end with `TERM=term`. When `cwd`
  /// is set the child starts there, so a new window inherits its parent's cwd.
  #[expect(
    clippy::disallowed_methods,
    unsafe_code,
    reason = "launching the configured shell and configuring its controlling \
              terminal are PTY boundaries"
  )]
  pub fn spawn(
    cols: u16,
    rows: u16,
    cell: (u16, u16),
    term: &str,
    cwd: Option<&Path>,
    env: &[(String, String)],
  ) -> anyhow::Result<Self> {
    let master =
      openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
        .context("open pty master")?;
    grantpt(&master).context("grantpt")?;
    unlockpt(&master).context("unlockpt")?;
    let slave =
      ioctl_tiocgptpeer(&master, OpenptFlags::RDWR | OpenptFlags::NOCTTY)
        .context("open pty slave")?;

    set_winsize(&master, cols, rows, cell)?;

    // Resolve the shell the way Ghostty and WezTerm do: `$SHELL` first, then
    // the passwd entry, then `/bin/sh`. A terminal launched from a display
    // manager often has no `SHELL` in its environment, so without the passwd
    // lookup the fallback would ignore the user's configured login shell.
    let shell = std::env::var_os("SHELL")
      .filter(|s| !s.is_empty())
      .or_else(passwd_shell)
      .unwrap_or_else(|| "/bin/sh".into());
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
    cmd
      .arg0(&argv0)
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
            .env("COLORTERM", "truecolor")
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
  pub const fn master(&self) -> &OwnedFd {
    &self.master
  }

  /// Inform the kernel (and thus the child) of a new terminal size; `cell` is
  /// the cell size in pixels, carried into the winsize pixel fields.
  pub fn resize(
    &self,
    cols: u16,
    rows: u16,
    cell: (u16, u16),
  ) -> anyhow::Result<()> {
    set_winsize(&self.master, cols, rows, cell)
  }

  /// Reap the child if it has exited.
  pub fn wait(&mut self) -> io::Result<ExitStatus> {
    self.child.wait()
  }

  /// Reap the child without blocking, returning its status if it has exited.
  pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
    self.child.try_wait()
  }
}

fn set_winsize(
  master: &OwnedFd,
  cols: u16,
  rows: u16,
  cell: (u16, u16),
) -> anyhow::Result<()> {
  let ws = Winsize {
    ws_row:    rows,
    ws_col:    cols,
    // The pixel geometry lets graphics-protocol clients size images; it is
    // the grid size times the cell size.
    ws_xpixel: cols.saturating_mul(cell.0),
    ws_ypixel: rows.saturating_mul(cell.1),
  };
  tcsetwinsize(master.as_fd(), ws).context("set pty winsize")
}

/// The current user's login shell from the passwd database via `getpwuid_r`.
///
/// This is the fallback when `$SHELL` is unset, e.g., a terminal launched from
/// a display manager, which does not export `SHELL` so the user's configured
/// login shell is honoured rather than defaulting to `/bin/sh`. NSS-aware, like
/// Ghostty and `WezTerm`.
///
/// # Returns
///
/// `None` on any lookup error or an empty shell.
#[expect(
  unsafe_code,
  reason = "getpwuid_r is a libc FFI call with no safe binding in our deps" // boo rustix
)]
fn passwd_shell() -> Option<OsString> {
  use std::{ffi::CStr, mem, os::unix::ffi::OsStrExt, ptr};

  // SAFETY: getuid takes no arguments and is always successful.
  let uid = unsafe { libc::getuid() };
  let mut buf = vec![0u8; 1024];
  loop {
    // SAFETY: a zeroed passwd is a valid initial state (null pointers);
    // getpwuid_r fully populates it on success and we never read it on failure.
    let mut pwd: libc::passwd = unsafe { mem::zeroed() };
    let mut result: *mut libc::passwd = ptr::null_mut();
    // SAFETY: `pwd` and `result` are valid out-pointers; `buf` provides
    // `buf.len()` writable bytes. getpwuid_r writes only within them and sets
    // `result` to `&pwd` on success or NULL when there is no entry.
    let rc = unsafe {
      libc::getpwuid_r(
        uid,
        &raw mut pwd,
        buf.as_mut_ptr().cast::<libc::c_char>(),
        buf.len(),
        &raw mut result,
      )
    };
    // The buffer was too small: grow it (bounded) and retry.
    if rc == libc::ERANGE && buf.len() < (1 << 20) {
      buf.resize(buf.len() * 2, 0);
      continue;
    }
    if rc != 0 || result.is_null() || pwd.pw_shell.is_null() {
      return None;
    }
    // SAFETY: on success `pw_shell` is a NUL-terminated C string within `buf`,
    // valid until `buf` is dropped; the bytes are copied into the owned result.
    let bytes = unsafe { CStr::from_ptr(pwd.pw_shell) }.to_bytes();
    return (!bytes.is_empty()).then(|| OsStr::from_bytes(bytes).to_owned());
  }
}

/// Login-shell `argv[0]` is the shell's basename with a leading `-`.
fn login_argv0(shell: &OsStr) -> OsString {
  let name = Path::new(shell)
    .file_name()
    .unwrap_or_else(|| OsStr::new("sh"));
  let mut argv0 = OsString::from("-");
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

  #[test]
  fn passwd_shell_lookup_is_sane() {
    // The call must never panic. When the current user has a passwd entry, its
    // shell is a non-empty absolute path.
    if let Some(shell) = passwd_shell() {
      assert!(!shell.is_empty());
      assert!(Path::new(&shell).is_absolute(), "{shell:?} not absolute");
    }
  }
}
