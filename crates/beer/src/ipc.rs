//! Daemon-mode's IPC is a small length-prefixed protocol over a Unix socket.
//!
//! The server (`--server`) hosts terminal windows in one process; a thin client
//! forwards its working directory and environment, then blocks until its window
//! closes and exits with the same status. The framing is deliberately tiny: one
//! length-prefixed request from client to server, then a one-byte exit status
//! back when that window closes.

use std::{
  io::{self, Read, Write},
  os::unix::net::UnixStream,
  path::PathBuf,
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

/// The daemon socket on `$XDG_RUNTIME_DIR/beer-$WAYLAND_DISPLAY.sock`, falling
/// back to a temp dir and `wayland-0` when those variables are unset.
pub fn socket_path() -> PathBuf {
  let dir = std::env::var_os("XDG_RUNTIME_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(std::env::temp_dir);
  let display =
    std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
  dir.join(format!("beer-{display}.sock"))
}

impl OpenRequest {
  /// Encode as a length-prefixed frame: a big-endian `u32` body length, then
  /// the body. Body layout: `[u32 cwd_len][cwd utf8] [u32 env_count]` followed
  /// by `env_count` pairs of `[u32 klen][k][u32 vlen][v]`.
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
  let mut stream = UnixStream::connect(socket_path())?;
  stream.write_all(&req.encode())?;
  let mut code = [0u8; 1];
  match stream.read_exact(&mut code) {
    Ok(()) => Ok(code[0]),
    // The server closed without a status (e.g. window killed), treat as ok.
    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
    Err(e) => Err(e),
  }
}

/// Read one request frame from an accepted (blocking) client stream.
///
/// # Errors
///
/// Fails with [`io::ErrorKind::InvalidData`] when the length prefix exceeds the
/// frame cap or the body is malformed, and propagates any underlying read
/// error.
pub fn read_request(stream: &mut UnixStream) -> io::Result<OpenRequest> {
  let mut len = [0u8; 4];
  stream.read_exact(&mut len)?;
  let n = u32::from_be_bytes(len) as usize;
  if n > MAX_REQUEST {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "request too large",
    ));
  }
  let mut body = vec![0u8; n];
  stream.read_exact(&mut body)?;
  OpenRequest::decode(&body).ok_or_else(|| {
    io::Error::new(io::ErrorKind::InvalidData, "malformed request")
  })
}

/// Server: send the final exit status to a client and drop the connection.
pub fn send_exit(mut stream: UnixStream, code: u8) {
  let _ = stream.write_all(&[code]);
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
  buf.extend_from_slice(&v.to_be_bytes());
}

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
}
