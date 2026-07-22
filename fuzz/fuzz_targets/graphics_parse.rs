#![no_main]

use libfuzzer_sys::fuzz_target;

// The kitty graphics APC control block is attacker-controllable escape data;
// the key/value parse must tolerate any byte sequence.
fuzz_target!(|data: &[u8]| {
  let _ = beer_protocols::graphics::parse(data);
});
