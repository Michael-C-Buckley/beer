use std::io::Cursor;

use beer_protocols::codec::base64_encode;

use super::{
  decode::{check_size, read_limited},
  *,
};

fn b64(data: &[u8]) -> Vec<u8> {
  base64_encode(data).into_bytes()
}

fn rgba_cmd(w: u32, h: u32, id: u32, action: Action) -> GraphicsCommand {
  GraphicsCommand {
    action,
    format: Format::Rgba,
    width: w,
    height: h,
    id,
    ..Default::default()
  }
}

#[test]
fn transmit_rgba_stores_and_acks() {
  let mut g = Graphics::new();
  let px = vec![0xAB; 2 * 2 * 4];
  let out = g.handle(rgba_cmd(2, 2, 1, Action::Transmit), &b64(&px), (8, 16));
  assert_eq!(out.response.as_deref(), Some(&b"\x1b_Gi=1;OK\x1b\\"[..]));
  let img = g.image(1).expect("stored");
  assert_eq!((img.width, img.height), (2, 2));
  assert_eq!(img.current_rgba().len(), 16);
}

#[test]
fn rgb_expands_to_rgba() {
  let mut g = Graphics::new();
  let px = vec![0x10; 2 * 3]; // 2x1 RGB
  let mut cmd = rgba_cmd(2, 1, 5, Action::Transmit);
  cmd.format = Format::Rgb;
  g.handle(cmd, &b64(&px), (8, 16));
  let img = g.image(5).unwrap();
  assert_eq!(img.current_rgba(), [
    0x10, 0x10, 0x10, 0xFF, 0x10, 0x10, 0x10, 0xFF
  ]);
}

#[test]
fn oversized_dimensions_are_rejected_without_wrapping() {
  assert!(check_size(u32::MAX, u32::MAX).is_err());

  let mut g = Graphics::new();
  let out = g.handle(
    rgba_cmd(u32::MAX, u32::MAX, 7, Action::Transmit),
    &[],
    (8, 16),
  );
  assert!(out.response.as_deref().is_some_and(|response| {
    response.windows(b"EINVAL".len()).any(|w| w == b"EINVAL")
  }));
  assert!(g.image(7).is_none());
}

#[test]
fn aspect_ratio_math_clamps_huge_placement_dimensions() {
  let (_, rows) = cell_rect(u32::MAX, 0, u32::MAX, u32::MAX, u32::MAX, 1);
  assert_eq!(rows, u32::MAX as usize);
}

#[test]
fn bounded_reader_rejects_data_past_its_limit() {
  let mut data = Cursor::new(b"four".as_slice());
  assert!(read_limited(&mut data, 3).is_err());
}

#[test]
fn transmit_and_display_emits_placement() {
  let mut g = Graphics::new();
  let px = vec![0; 16 * 16 * 4];
  let out = g.handle(
    rgba_cmd(16, 16, 2, Action::TransmitAndDisplay),
    &b64(&px),
    (8, 16),
  );
  match out.grid_op {
    Some(GridOp::Place {
      image, cols, rows, ..
    }) => {
      assert_eq!(image, 2);
      assert_eq!(cols, 2); // 16px / 8px cell
      assert_eq!(rows, 1); // 16px / 16px cell
    },
    other => panic!("expected placement, got {other:?}"),
  }
  assert!(g.placement(2, 0).is_some());
}

#[test]
fn chunked_direct_transmission_assembles() {
  let mut g = Graphics::new();
  let px = vec![0x7F; 4 * 4]; // 4x1 RGBA
  let full = base64_encode(&px).into_bytes();
  let (a, b) = full.split_at(8); // 8 is a multiple of 4
  let mut first = rgba_cmd(4, 1, 9, Action::Transmit);
  first.more = true;
  assert!(g.handle(first, a, (8, 16)).response.is_none());
  // Continuation carries only m=0.
  let last = GraphicsCommand {
    action: Action::Transmit,
    more: false,
    ..Default::default()
  };
  let out = g.handle(last, b, (8, 16));
  assert_eq!(out.response.as_deref(), Some(&b"\x1b_Gi=9;OK\x1b\\"[..]));
  assert_eq!(g.image(9).unwrap().current_rgba().len(), 16);
}

#[test]
fn query_verifies_without_storing() {
  let mut g = Graphics::new();
  let px = vec![0; 2 * 2 * 4];
  let out = g.handle(rgba_cmd(2, 2, 3, Action::Query), &b64(&px), (8, 16));
  assert_eq!(out.response.as_deref(), Some(&b"\x1b_Gi=3;OK\x1b\\"[..]));
  assert!(g.image(3).is_none());
}

#[test]
fn bad_payload_reports_error() {
  let mut g = Graphics::new();
  let out = g.handle(rgba_cmd(100, 100, 1, Action::Transmit), b"!!!!", (8, 16));
  let resp = out.response.expect("error response");
  assert!(resp.starts_with(b"\x1b_Gi=1;"));
  assert!(resp.windows(6).any(|w| w == b"EINVAL"));
}

#[test]
fn quiet_suppresses_success() {
  let mut g = Graphics::new();
  let px = vec![0; 4];
  let mut cmd = rgba_cmd(1, 1, 1, Action::Transmit);
  cmd.quiet = 1;
  assert!(g.handle(cmd, &b64(&px), (8, 16)).response.is_none());
}

#[test]
fn delete_all_clears() {
  let mut g = Graphics::new();
  g.handle(
    rgba_cmd(2, 2, 1, Action::TransmitAndDisplay),
    &b64(&[0; 16]),
    (8, 16),
  );
  let cmd = GraphicsCommand {
    action: Action::Delete,
    delete: b'A',
    ..Default::default()
  };
  let out = g.handle(cmd, &[], (8, 16));
  assert!(matches!(
    out.grid_op,
    Some(GridOp::Clear {
      spec: ClearSpec::All,
      free: true,
    })
  ));
  g.finish_delete(&[(1, 0)], true, |_| false);
  assert!(g.image(1).is_none(), "uppercase delete frees data");
}

#[test]
fn delete_targets_remain_precise() {
  let mut graphics = Graphics::new();
  let cmd = GraphicsCommand {
    action: Action::Delete,
    delete: b'y',
    y: 3,
    ..Default::default()
  };
  assert!(matches!(
    graphics.handle(cmd, &[], (8, 16)).grid_op,
    Some(GridOp::Clear {
      spec: ClearSpec::Row(3),
      free: false,
    })
  ));
}

#[test]
fn relative_placement_keeps_parent_and_offsets() {
  let mut graphics = Graphics::new();
  let pixels = b64(&[0; 16]);
  let mut parent = rgba_cmd(2, 2, 1, Action::TransmitAndDisplay);
  parent.placement = 7;
  graphics.handle(parent, &pixels, (8, 16));
  let mut child = rgba_cmd(2, 2, 2, Action::TransmitAndDisplay);
  child.placement = 8;
  child.parent_id = 1;
  child.parent_placement = 7;
  child.rel_h = 2;
  child.rel_v = -1;
  let outcome = graphics.handle(child, &pixels, (8, 16));
  assert!(matches!(
    outcome.grid_op,
    Some(GridOp::Place {
      parent: Some((1, 7, 2, -1)),
      ..
    })
  ));
}

#[test]
fn animation_frames_advance_on_tick() {
  let mut g = Graphics::new();
  // Root frame: a 1x1 red pixel.
  g.handle(
    rgba_cmd(1, 1, 1, Action::Transmit),
    &b64(&[0xFF, 0, 0, 0xFF]),
    (8, 16),
  );
  // Append a second frame (a=f): a 1x1 green pixel, default 40ms gap.
  let frame = GraphicsCommand {
    action: Action::Frame,
    format: Format::Rgba,
    width: 1,
    height: 1,
    id: 1,
    ..Default::default()
  };
  g.handle(frame, &b64(&[0, 0xFF, 0, 0xFF]), (8, 16));
  // Run looping (a=a, s=3).
  let run = GraphicsCommand {
    action: Action::Animate,
    id: 1,
    width: 3,
    ..Default::default()
  };
  g.handle(run, &[], (8, 16));
  assert!(g.is_animating());
  assert_eq!(&g.image(1).unwrap().current_rgba()[..4], &[
    0xFF, 0, 0, 0xFF
  ]);
  // A short tick does not cross the 40ms gap; a full one advances a frame.
  assert!(!g.tick(10));
  assert!(g.tick(40));
  assert_eq!(&g.image(1).unwrap().current_rgba()[..4], &[
    0, 0xFF, 0, 0xFF
  ]);
}

#[test]
fn animate_selects_current_frame() {
  let mut g = Graphics::new();
  g.handle(
    rgba_cmd(1, 1, 1, Action::Transmit),
    &b64(&[1, 1, 1, 0xFF]),
    (8, 16),
  );
  let frame = GraphicsCommand {
    action: Action::Frame,
    format: Format::Rgba,
    width: 1,
    height: 1,
    id: 1,
    ..Default::default()
  };
  g.handle(frame, &b64(&[2, 2, 2, 0xFF]), (8, 16));
  // a=a,c=2 makes the second frame current without playing.
  let select = GraphicsCommand {
    action: Action::Animate,
    id: 1,
    c: 2,
    ..Default::default()
  };
  g.handle(select, &[], (8, 16));
  assert_eq!(&g.image(1).unwrap().current_rgba()[..4], &[2, 2, 2, 0xFF]);
  assert!(
    !g.is_animating(),
    "selecting a frame does not start playback"
  );
}
