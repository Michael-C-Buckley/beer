#![no_main]

use libfuzzer_sys::fuzz_target;

// OSC 7 delivers a percent-encoded `file://` URI from the shell; parsing it
// (which drives percent_decode) must be panic-free on arbitrary bytes.
fuzz_target!(|data: &[u8]| {
  let _ = beer_protocols::codec::file_uri_path(data);
  let _ = beer_protocols::codec::percent_decode(data);
});
