//! Byte codecs the escape-sequence layer leans on: base64 (OSC 52 clipboard),
//! hex (XTGETTCAP capability names), and percent-decoding of `file://` URIs
//! (OSC 7 working-directory reports).

const B64: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encode (used for OSC 52 query replies).
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
  let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
  for chunk in data.chunks(3) {
    let b = [
      chunk[0],
      *chunk.get(1).unwrap_or(&0),
      *chunk.get(2).unwrap_or(&0),
    ];
    let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
    out.push(B64[((n >> 18) & 63) as usize] as char);
    out.push(B64[((n >> 12) & 63) as usize] as char);
    out.push(if chunk.len() > 1 {
      B64[((n >> 6) & 63) as usize] as char
    } else {
      '='
    });
    out.push(if chunk.len() > 2 {
      B64[(n & 63) as usize] as char
    } else {
      '='
    });
  }
  out
}

/// Standard base64 decode, ignoring padding and whitespace; `None` on a bad
/// character.
#[must_use]
pub fn base64_decode(data: &[u8]) -> Option<Vec<u8>> {
  let val = |c: u8| -> Option<u32> {
    match c {
      b'A'..=b'Z' => Some(u32::from(c - b'A')),
      b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
      b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
      b'+' => Some(62),
      b'/' => Some(63),
      _ => None,
    }
  };
  let filtered: Vec<u8> = data
    .iter()
    .copied()
    .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
    .collect();
  let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
  for chunk in filtered.chunks(4) {
    if chunk.len() == 1 {
      return None; // a lone sextet cannot form a byte
    }
    let mut n = 0u32;
    for &c in chunk {
      n = (n << 6) | val(c)?;
    }
    n <<= 6 * (4 - u32::try_from(chunk.len()).ok()?);
    out.push(u8::try_from((n >> 16) & u32::from(u8::MAX)).ok()?);
    if chunk.len() >= 3 {
      out.push(u8::try_from((n >> 8) & u32::from(u8::MAX)).ok()?);
    }
    if chunk.len() >= 4 {
      out.push(u8::try_from(n & u32::from(u8::MAX)).ok()?);
    }
  }
  Some(out)
}

/// Decode an even-length lowercase/uppercase hex string into bytes (XTGETTCAP
/// names arrive hex-encoded).
#[must_use]
pub fn decode_hex(s: &[u8]) -> Option<Vec<u8>> {
  if s.is_empty() || !s.len().is_multiple_of(2) {
    return None;
  }

  s.chunks_exact(2)
    .map(|pair| Some((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?))
    .collect()
}

/// Turn a hexadecimal character into its numerical value.
fn hex_nibble(b: u8) -> Option<u8> {
  (b as char).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

/// Percent-decode `%XX` byte escapes in a URI path, passing other bytes
/// through.
#[must_use]
pub fn percent_decode(s: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(s.len());
  let mut i = 0;
  while i < s.len() {
    if s[i] == b'%' && i + 2 < s.len() {
      let hi = hex_nibble(s[i + 1]);
      let lo = hex_nibble(s[i + 2]);
      if let (Some(hi), Some(lo)) = (hi, lo) {
        out.push((hi << 4) | lo);
        i += 3;
        continue;
      }
    }
    out.push(s[i]);
    i += 1;
  }
  out
}

/// Extract the local path from an OSC 7 `file://host/path` URI, percent-decoding
/// `%XX` escapes. The host part is ignored (we only spawn locally). Returns
/// `None` if it is not a usable absolute path.
#[must_use]
pub fn file_uri_path(uri: &[u8]) -> Option<String> {
  let rest = uri.strip_prefix(b"file://").unwrap_or(uri);
  // Skip the authority (host) up to the first '/', which begins the path.
  let slash = rest.iter().position(|&b| b == b'/')?;
  let path_bytes = percent_decode(&rest[slash..]);
  let path = String::from_utf8(path_bytes).ok()?;
  path.starts_with('/').then_some(path)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn base64_round_trips() {
    for s in [
      "",
      "f",
      "fo",
      "foo",
      "foob",
      "fooba",
      "foobar",
      "hi there\n",
    ] {
      let enc = base64_encode(s.as_bytes());
      assert_eq!(base64_decode(enc.as_bytes()).as_deref(), Some(s.as_bytes()));
    }
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64_decode(b"Zm9vYmFy").as_deref(), Some(&b"foobar"[..]));
  }

  #[test]
  fn decode_hex_rejects_odd_and_nonhex() {
    assert_eq!(decode_hex(b"544e").as_deref(), Some(&b"TN"[..]));
    assert_eq!(decode_hex(b"54e"), None);
    assert_eq!(decode_hex(b"zz"), None);
  }

  #[test]
  fn file_uri_decodes_percent_and_drops_host() {
    assert_eq!(
      file_uri_path(b"file://hermes/home/user/my%20dir").as_deref(),
      Some("/home/user/my dir")
    );
    assert_eq!(file_uri_path(b"file://host").as_deref(), None);
  }
}
