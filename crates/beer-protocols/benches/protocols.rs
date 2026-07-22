//! Micro-benchmarks for the escape-stream decode/encode hot paths.
//!
//! These cover the pure protocol codecs beer runs on every relevant byte of
//! terminal traffic: base64 (OSC 52 clipboard and graphics payloads), the kitty
//! graphics APC control parse, text-sizing (`OSC 66`) metadata, SGR extended
//! colour, and mouse-report encoding. The full VT-feed/grid/render pipeline is
//! profiled live against a running window; see `doc/performance.md`.

use std::hint::black_box;

use beer_protocols::{
  MouseEncoding,
  codec::{base64_decode, base64_encode},
  graphics,
  mouse::encode_mouse,
  sgr::ext_color,
  text_size,
};
use criterion::{Criterion, criterion_group, criterion_main};
use smithay_client_toolkit::seat::keyboard::Modifiers;

fn bench_base64(c: &mut Criterion) {
  // A 64 KiB payload, the scale of a graphics transmission chunk.
  let data = vec![0xA5u8; 64 * 1024];
  let encoded = base64_encode(&data);
  c.bench_function("base64_encode_64k", |b| {
    b.iter(|| base64_encode(black_box(&data)));
  });
  c.bench_function("base64_decode_64k", |b| {
    b.iter(|| base64_decode(black_box(encoded.as_bytes())));
  });
}

fn bench_graphics_parse(c: &mut Criterion) {
  // A representative kitty graphics control block (transmit + display).
  let control = b"a=T,f=32,s=64,v=64,i=1,p=1,z=0,q=2";
  c.bench_function("graphics_parse", |b| {
    b.iter(|| graphics::parse(black_box(control)));
  });
}

fn bench_text_size_parse(c: &mut Criterion) {
  let meta = b"s=2:w=3:n=1:d=2:v=1:h=1";
  c.bench_function("text_size_parse", |b| {
    b.iter(|| text_size::parse(black_box(meta)));
  });
}

fn bench_sgr_ext_color(c: &mut Criterion) {
  // Truecolor foreground: SGR 38;2;R;G;B, split into the parameter groups
  // `ext_color` consumes.
  let p0: &[u16] = &[38];
  let p1: &[u16] = &[2];
  let p2: &[u16] = &[10];
  let p3: &[u16] = &[20];
  let p4: &[u16] = &[30];
  let items: Vec<&[u16]> = vec![p0, p1, p2, p3, p4];
  c.bench_function("sgr_ext_color_truecolor", |b| {
    b.iter(|| ext_color(black_box(&items), black_box(0)));
  });
}

fn bench_mouse_encode(c: &mut Criterion) {
  let mods = Modifiers::default();
  c.bench_function("mouse_encode_sgr", |b| {
    b.iter(|| {
      encode_mouse(
        black_box(MouseEncoding::Sgr),
        black_box(0),
        black_box(120),
        black_box(40),
        black_box(true),
        black_box(false),
        black_box(mods),
      )
    });
  });
}

criterion_group!(
  benches,
  bench_base64,
  bench_graphics_parse,
  bench_text_size_parse,
  bench_sgr_ext_color,
  bench_mouse_encode,
);
criterion_main!(benches);
