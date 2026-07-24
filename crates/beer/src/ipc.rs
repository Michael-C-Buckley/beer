//! Daemon-mode's IPC is a small length-prefixed protocol over a Unix socket.
//!
//! The server (`--server`) hosts terminal windows in one process; a thin client
//! forwards its working directory and environment, then blocks until its window
//! closes and exits with the same status. The framing is deliberately tiny: one
//! length-prefixed request from client to server, then a one-byte exit status
//! back when that window closes.

use std::{
  fs,
  io::{self, Read, Write},
  os::unix::{
    fs::{FileTypeExt, MetadataExt, PermissionsExt},
    net::{UnixListener, UnixStream},
  },
  path::{Path, PathBuf},
};

/// Upper bound on a request frame, so a bad client cannot make us allocate
/// wildly.
const MAX_REQUEST: usize = 1 << 20;

/// A request to open a window in the server.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OpenRequest {
  /// Directory the child shell starts in (the client's cwd); `None` inherits.
  pub cwd: Option<String>,
  /// Environment for the child shell (the client's environment).
  pub env: Vec<(String, String)>,
}

/// The daemon socket on `$XDG_RUNTIME_DIR/beer-$WAYLAND_DISPLAY.sock`.
///
/// Daemon IPC carries a client's environment and starts a shell as the server,
/// so it must live in the user's private runtime directory rather than a
/// shared temp directory.
#[expect(
  clippy::absolute_paths,
  reason = "daemon discovery intentionally uses the process environment and \
            runtime directory"
)]
pub fn socket_path() -> io::Result<PathBuf> {
  let dir = std::env::var_os("XDG_RUNTIME_DIR")
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::NotFound,
        "XDG_RUNTIME_DIR is required for daemon IPC",
      )
    })?;
  let metadata = fs::metadata(&dir)?;
  if !metadata.is_dir()
    || metadata.uid() != rustix::process::getuid().as_raw()
    || metadata.mode() & 0o077 != 0
  {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "XDG_RUNTIME_DIR must be a private directory owned by this user",
    ));
  }
  let display =
    std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
  socket_path_in(&dir, &display)
}

/// Bind the daemon listener with owner-only access, replacing only a stale
/// socket at its exact path.
pub fn bind_listener() -> io::Result<(UnixListener, PathBuf)> {
  let path = socket_path()?;
  let listener = bind_listener_at(&path)?;
  Ok((listener, path))
}

fn socket_path_in(dir: &Path, display: &str) -> io::Result<PathBuf> {
  if display.is_empty() || display.contains('/') {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "WAYLAND_DISPLAY must be a socket name",
    ));
  }
  Ok(dir.join(format!("beer-{display}.sock")))
}

fn bind_listener_at(path: &Path) -> io::Result<UnixListener> {
  match UnixStream::connect(path) {
    Ok(_) => {
      return Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("a beer server is already running at {}", path.display()),
      ));
    },
    Err(err)
      if matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
      ) => {},
    Err(err) => return Err(err),
  }
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
    Ok(_) => {
      return Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("refusing to replace non-socket {}", path.display()),
      ));
    },
    Err(err) if err.kind() == io::ErrorKind::NotFound => {},
    Err(err) => return Err(err),
  }
  let listener = UnixListener::bind(path)?;
  restrict_socket_permissions(path)?;
  Ok(listener)
}

fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
  fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

impl OpenRequest {
  /// Encode as a length-prefixed frame: a big-endian `u32` body length, then
  /// the body. Body layout: `[u32 cwd_len][cwd utf8] [u32 env_count]` followed
  /// by `env_count` pairs of `[u32 klen][k][u32 vlen][v]`.
  #[expect(
    clippy::cast_possible_truncation,
    reason = "the IPC wire format deliberately uses u32 length fields"
  )]
  pub fn encode(&self) -> Vec<u8> {
    let mut body = Vec::new();
    put_bytes(&mut body, self.cwd.as_deref().unwrap_or("").as_bytes());
    put_u32(&mut body, self.env.len() as u32);
    for (k, v) in &self.env {
      put_bytes(&mut body, k.as_bytes());
      put_bytes(&mut body, v.as_bytes());
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    put_u32(&mut frame, body.len() as u32);
    frame.extend_from_slice(&body);
    frame
  }

  /// Parse a frame body (the bytes after the length prefix) into a request.
  /// Truncated or malformed input yields `None`.
  pub fn decode(buf: &[u8]) -> Option<Self> {
    let mut rest = buf;
    let cwd_bytes = take_bytes(&mut rest)?;
    // An empty cwd field means "inherit"; non-UTF-8 also degrades to inherit.
    let cwd = if cwd_bytes.is_empty() {
      None
    } else {
      Some(String::from_utf8(cwd_bytes.to_vec()).ok()?)
    };
    let count = take_u32(&mut rest)? as usize;
    // Reject an impossible pair count before reserving.
    if count > buf.len() {
      return None;
    }
    let mut env = Vec::with_capacity(count);
    for _ in 0..count {
      let k = String::from_utf8(take_bytes(&mut rest)?.to_vec()).ok()?;
      let v = String::from_utf8(take_bytes(&mut rest)?.to_vec()).ok()?;
      env.push((k, v));
    }
    Some(Self { cwd, env })
  }
}

/// Client: connect to a running server, send `req`, and block until the server
/// sends the one-byte exit status.
///
/// # Errors
///
/// Returns an error if no server is listening.
pub fn run_client(req: &OpenRequest) -> io::Result<u8> {
  let mut stream = UnixStream::connect(socket_path()?)?;
  stream.write_all(&req.encode())?;
  let mut code = [0u8; 1];
  match stream.read_exact(&mut code) {
    Ok(()) => Ok(code[0]),
    // The server closed without a status (e.g. window killed), treat as ok.
    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
    Err(e) => Err(e),
  }
}

/// Incrementally read one request frame from a non-blocking client stream.
#[derive(Default)]
pub struct RequestReader {
  len:       [u8; 4],
  len_read:  usize,
  body:      Option<Vec<u8>>,
  body_read: usize,
}

impl RequestReader {
  /// Consume available bytes. `Ok(None)` means the frame is incomplete.
  pub fn read_from<R: Read>(
    &mut self,
    mut stream: R,
  ) -> io::Result<Option<OpenRequest>> {
    loop {
      if self.len_read < self.len.len() {
        match stream.read(&mut self.len[self.len_read..]) {
          Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
          Ok(n) => self.len_read += n,
          Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            return Ok(None);
          },
          Err(err) => return Err(err),
        }
        continue;
      }

      if self.body.is_none() {
        let len = u32::from_be_bytes(self.len) as usize;
        if len > MAX_REQUEST {
          return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request too large",
          ));
        }
        self.body = Some(vec![0; len]);
      }

      let Some(body) = self.body.as_mut() else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "request body was not initialized",
        ));
      };
      if self.body_read == body.len() {
        return OpenRequest::decode(body).map(Some).ok_or_else(|| {
          io::Error::new(io::ErrorKind::InvalidData, "malformed request")
        });
      }
      match stream.read(&mut body[self.body_read..]) {
        Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        Ok(n) => self.body_read += n,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(err) => return Err(err),
      }
    }
  }
}

/// Server: send the final exit status to a client and drop the connection.
pub fn send_exit(mut stream: UnixStream, code: u8) {
  let _ = stream.write_all(&[code]);
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
  buf.extend_from_slice(&v.to_be_bytes());
}

#[expect(
  clippy::cast_possible_truncation,
  reason = "the IPC wire format deliberately uses u32 length fields"
)]
fn put_bytes(buf: &mut Vec<u8>, data: &[u8]) {
  put_u32(buf, data.len() as u32);
  buf.extend_from_slice(data);
}

/// Read a big-endian `u32` off the front of `buf`, advancing it.
fn take_u32(buf: &mut &[u8]) -> Option<u32> {
  let (head, tail) = buf.split_at_checked(4)?;
  *buf = tail;
  Some(u32::from_be_bytes([head[0], head[1], head[2], head[3]]))
}

/// Read a length-prefixed byte slice off the front of `buf`, advancing it.
fn take_bytes<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
  let n = take_u32(buf)? as usize;
  let (head, tail) = buf.split_at_checked(n)?;
  *buf = tail;
  Some(head)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Default)]
  struct NonblockingReader {
    bytes: Vec<u8>,
    pos:   usize,
  }

  impl NonblockingReader {
    fn push(&mut self, bytes: &[u8]) {
      self.bytes.extend_from_slice(bytes);
    }
  }

  impl Read for NonblockingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
      let Some(available) = self.bytes.get(self.pos..) else {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
      };
      if available.is_empty() {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
      }
      let len = buf.len().min(available.len());
      buf[..len].copy_from_slice(&available[..len]);
      self.pos += len;
      Ok(len)
    }
  }

  fn round_trip(req: &OpenRequest) {
    let frame = req.encode();
    // The frame is a u32 length prefix followed by the body.
    let len =
      u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    assert_eq!(len, frame.len() - 4);
    assert_eq!(OpenRequest::decode(&frame[4..]).as_ref(), Some(req));
  }

  #[test]
  fn empty_request_round_trips() {
    round_trip(&OpenRequest::default());
  }

  #[test]
  fn cwd_only_round_trips() {
    round_trip(&OpenRequest {
      cwd: Some("/home/user/project".into()),
      env: Vec::new(),
    });
  }

  #[test]
  fn env_only_round_trips() {
    round_trip(&OpenRequest {
      cwd: None,
      env: vec![
        ("TERM".into(), "beer".into()),
        ("PATH".into(), "/bin".into()),
      ],
    });
  }

  #[test]
  fn cwd_and_env_round_trip() {
    round_trip(&OpenRequest {
      cwd: Some("/tmp".into()),
      env: vec![("KEY".into(), "value with spaces".into())],
    });
  }

  #[test]
  fn decode_rejects_truncated() {
    let frame = OpenRequest {
      cwd: Some("/tmp".into()),
      env: vec![("A".into(), "B".into())],
    }
    .encode();
    // Chop the body short; decode must reject rather than panic.
    assert_eq!(OpenRequest::decode(&frame[4..frame.len() - 3]), None);
    assert_eq!(OpenRequest::decode(&[0, 0, 0]), None);
  }

  #[test]
  fn request_reader_waits_for_a_complete_nonblocking_frame() {
    let request = OpenRequest {
      cwd: Some("/tmp".into()),
      env: vec![("TERM".into(), "beer".into())],
    };
    let frame = request.encode();
    let mut stream = NonblockingReader::default();
    stream.push(&frame[..2]);

    let mut reader = RequestReader::default();
    assert_eq!(reader.read_from(&mut stream).unwrap(), None);

    stream.push(&frame[2..]);
    assert_eq!(reader.read_from(&mut stream).unwrap(), Some(request));
  }

  #[test]
  fn request_reader_rejects_an_oversized_frame_before_allocating() {
    let mut stream = NonblockingReader::default();
    stream.push(
      &(u32::try_from(MAX_REQUEST).unwrap_or(u32::MAX) + 1).to_be_bytes(),
    );

    let err = RequestReader::default().read_from(&mut stream).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
  }

  #[test]
  fn socket_name_rejects_path_separators() {
    let err =
      socket_path_in(Path::new("/run/user/1000"), "../other").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  #[expect(
    clippy::absolute_paths,
    reason = "the test intentionally exercises host filesystem metadata and \
              temporary paths"
  )]
  fn socket_permission_helper_is_owner_only() {
    use std::os::unix::fs::MetadataExt as _;

    let path = std::env::temp_dir().join(format!(
      "beer-ipc-test-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos(),
    ));
    fs::File::create(&path).unwrap();
    restrict_socket_permissions(&path).unwrap();

    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

    fs::remove_file(path).unwrap();
  }
}
