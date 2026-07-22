#![no_main]

use libfuzzer_sys::fuzz_target;

// XTGETTCAP capability names arrive hex-encoded; decode_hex must reject
// malformed input rather than panic.
fuzz_target!(|data: &[u8]| {
  let _ = beer_protocols::codec::decode_hex(data);
});
