#![no_main]

use libfuzzer_sys::fuzz_target;

// The text-sizing (`OSC 66`) metadata block is untrusted escape data; parsing
// it must never panic regardless of the key/value bytes supplied.
fuzz_target!(|data: &[u8]| {
  let _ = beer_protocols::text_size::parse(data);
});
