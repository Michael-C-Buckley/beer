#![no_main]

use libfuzzer_sys::fuzz_target;

// OSC 52 clipboard payloads and graphics transmissions arrive base64-encoded
// from untrusted programs; the decoder must never panic on malformed input.
fuzz_target!(|data: &[u8]| {
  let _ = beer_protocols::codec::base64_decode(data);
});
