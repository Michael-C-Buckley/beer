//! sctk `Handler`/`Dispatch` impls on [`WaylandState`]. Input and lifecycle
//! events are translated into neutral [`beer_window`] events and handed to the
//! app (`self.app.on_*(&mut self.plat, …)`); platform bookkeeping (seats,
//! surfaces, scale, clipboard serving) stays here on `self.plat`.

use beer_window::{
  ImeEvent,
  PointerButton,
  PointerEvent as WEvent,
  Scroll,
  TouchEvent,
  WindowId,
};
use smithay_client_toolkit::{
  activation::{ActivationHandler, RequestData},
  compositor::CompositorHandler,
  data_device_manager::{
    WritePipe,
    data_device::DataDeviceHandler,
    data_offer::{DataOfferHandler, DragOffer},
    data_source::DataSourceHandler,
  },
  output::{OutputHandler, OutputState},
  primary_selection::{
    device::PrimarySelectionDeviceHandler,
    selection::PrimarySelectionSourceHandler,
  },
  reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
  },
  registry::{ProvidesRegistryState, RegistryState},
  registry_handlers,
  seat::{
    Capability,
    SeatHandler,
    SeatState,
    keyboard::{
      KeyEvent,
      KeyboardHandler,
      Keysym,
      Modifiers,
      RawModifiers,
      RepeatInfo,
    },
    pointer::{
      BTN_LEFT,
      BTN_MIDDLE,
      BTN_RIGHT,
      PointerEvent,
      PointerEventKind,
      PointerHandler,
    },
    touch::TouchHandler,
  },
  shell::{
    WaylandSurface,
    xdg::window::{Window as XdgWindow, WindowConfigure, WindowHandler},
  },
  shm::{Shm, ShmHandler},
};
use wayland_client::{
  Connection,
  Dispatch,
  QueueHandle,
  protocol::{
    wl_data_device::WlDataDevice,
    wl_data_device_manager::DndAction,
    wl_data_source::WlDataSource,
    wl_keyboard::WlKeyboard,
    wl_output::{self, WlOutput},
    wl_pointer::WlPointer,
    wl_seat::WlSeat,
    wl_surface::WlSurface,
    wl_touch::WlTouch,
  },
};
use wayland_protocols::wp::{
  fractional_scale::v1::client::wp_fractional_scale_v1::{
    self,
    WpFractionalScaleV1,
  },
  text_input::zv3::client::zwp_text_input_v3::{
    self,
    ContentHint,
    ContentPurpose,
    ZwpTextInputV3,
  },
};

use crate::state::WaylandState;

/// Map a Wayland button code to a neutral [`PointerButton`].
#[expect(
  clippy::cast_possible_truncation,
  reason = "extra button codes above u16 are not bindable and clamp harmlessly"
)]
const fn button(code: u32) -> PointerButton {
  match code {
    BTN_LEFT => PointerButton::Left,
    BTN_MIDDLE => PointerButton::Middle,
    BTN_RIGHT => PointerButton::Right,
    other => PointerButton::Other(other as u16),
  }
}

impl CompositorHandler for WaylandState {
  fn scale_factor_changed(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    surface: &WlSurface,
    factor: i32,
  ) {
    // Integer fallback for compositors without fractional-scale-v1; ignored
    // when a fractional-scale object drives the scale instead.
    let Some(idx) = self.plat.window_index_for_surface(surface) else {
      return;
    };
    if self.plat.windows[idx].fractional_scale.is_none() {
      let scale = u32::try_from(factor.max(1))
        .unwrap_or(u32::MAX)
        .saturating_mul(120);
      let id = self.plat.windows[idx].id;
      self.plat.windows[idx].scale120 = scale;
      self.app.on_scale(&mut self.plat, id, scale);
    }
  }

  fn transform_changed(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlSurface,
    _: wl_output::Transform,
  ) {
  }

  fn frame(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    surface: &WlSurface,
    _: u32,
  ) {
    let Some(idx) = self.plat.window_index_for_surface(surface) else {
      return;
    };
    self.plat.windows[idx].frame_pending = false;
    // The compositor is ready for another frame; repaint if the app has asked.
    if self.plat.windows[idx].wants_draw {
      self.plat.windows[idx].wants_draw = false;
      let id = self.plat.windows[idx].id;
      self.app.render(&mut self.plat, id);
    }
  }

  fn surface_enter(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlSurface,
    _: &WlOutput,
  ) {
  }

  fn surface_leave(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlSurface,
    _: &WlOutput,
  ) {
  }
}

impl WindowHandler for WaylandState {
  fn request_close(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    window: &XdgWindow,
  ) {
    if let Some(id) = self.plat.window_id_for_surface(window.wl_surface()) {
      self.app.on_close(&mut self.plat, id);
    }
  }

  fn configure(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    window: &XdgWindow,
    configure: WindowConfigure,
    _serial: u32,
  ) {
    let Some(idx) = self.plat.window_index_for_surface(window.wl_surface())
    else {
      return;
    };
    if let (Some(w), Some(h)) = configure.new_size {
      self.plat.windows[idx].width = w.get();
      self.plat.windows[idx].height = h.get();
      if let Some(vp) = &self.plat.windows[idx].viewport {
        let (ww, hh) =
          (self.plat.windows[idx].width, self.plat.windows[idx].height);
        vp.set_destination(
          i32::try_from(ww.max(1)).unwrap_or(i32::MAX),
          i32::try_from(hh.max(1)).unwrap_or(i32::MAX),
        );
      }
    }
    let activated = configure.is_activated();
    let resizing = configure.is_resizing();
    self.plat.windows[idx].focused = activated;
    let (id, w, h) = {
      let win = &self.plat.windows[idx];
      (win.id, win.width, win.height)
    };
    self
      .app
      .on_configure(&mut self.plat, id, w, h, activated, resizing);
  }
}

impl ShmHandler for WaylandState {
  fn shm_state(&mut self) -> &mut Shm {
    &mut self.plat.shm
  }
}

impl SeatHandler for WaylandState {
  fn seat_state(&mut self) -> &mut SeatState {
    &mut self.plat.seat_state
  }

  fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat) {
    let i = self.plat.seat_index(&seat);
    self.plat.ensure_clipboard_devices(qh, &seat, i);
  }

  fn new_capability(
    &mut self,
    _: &Connection,
    qh: &QueueHandle<Self>,
    seat: WlSeat,
    capability: Capability,
  ) {
    let i = self.plat.seat_index(&seat);
    self.plat.ensure_clipboard_devices(qh, &seat, i);
    if capability == Capability::Keyboard
      && self.plat.seats[i].keyboard.is_none()
    {
      let loop_handle = self.plat.loop_handle.clone();
      let keyboard = self.plat.seat_state.get_keyboard_with_repeat(
        qh,
        &seat,
        None,
        loop_handle,
        Box::new(|state: &mut Self, kbd, event| {
          if let Some(id) = state.plat.focused_id() {
            let mods = state.plat.modifiers_for(kbd);
            state.app.on_key(&mut state.plat, id, &event, mods);
          }
        }),
      );
      match keyboard {
        Ok(keyboard) => self.plat.seats[i].keyboard = Some(keyboard),
        Err(err) => tracing::warn!("get keyboard: {err}"),
      }
      if self.plat.seats[i].text_input.is_none()
        && let Some(mgr) = self.plat.text_input_manager.as_ref()
      {
        self.plat.seats[i].text_input = Some(mgr.get_text_input(&seat, qh, ()));
      }
    }
    if capability == Capability::Pointer && self.plat.seats[i].pointer.is_none()
    {
      match self.plat.seat_state.get_pointer(qh, &seat) {
        Ok(pointer) => {
          self.plat.seats[i].cursor_shape_device = self
            .plat
            .cursor_shape_manager
            .as_ref()
            .map(|m| m.get_shape_device(&pointer, qh));
          self.plat.seats[i].pointer = Some(pointer);
        },
        Err(err) => tracing::warn!("get pointer: {err}"),
      }
    }
    if capability == Capability::Touch && self.plat.seats[i].touch.is_none() {
      match self.plat.seat_state.get_touch(qh, &seat) {
        Ok(touch) => self.plat.seats[i].touch = Some(touch),
        Err(err) => tracing::warn!("get touch: {err}"),
      }
    }
  }

  fn remove_capability(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    seat: WlSeat,
    capability: Capability,
  ) {
    let Some(s) = self.plat.seats.iter_mut().find(|s| s.seat == seat) else {
      return;
    };
    match capability {
      Capability::Keyboard => {
        if let Some(keyboard) = s.keyboard.take() {
          keyboard.release();
        }
      },
      Capability::Pointer => {
        s.cursor_shape_device = None;
        if let Some(pointer) = s.pointer.take() {
          pointer.release();
        }
      },
      Capability::Touch => {
        if let Some(touch) = s.touch.take() {
          touch.release();
        }
      },
      _ => {},
    }
    // A touch device vanishing mid-gesture must not leave stale scroll state.
    if capability == Capability::Touch {
      let wids: Vec<WindowId> =
        self.plat.touch_focus.drain().map(|(_, w)| w).collect();
      for wid in wids {
        self.app.on_touch(&mut self.plat, wid, TouchEvent::Cancel);
      }
    }
  }

  fn remove_seat(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    seat: WlSeat,
  ) {
    self.plat.seats.retain(|s| s.seat != seat);
    self.plat.active_seat = self
      .plat
      .active_seat
      .min(self.plat.seats.len().saturating_sub(1));
  }
}

impl KeyboardHandler for WaylandState {
  fn enter(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    keyboard: &WlKeyboard,
    surface: &WlSurface,
    serial: u32,
    _: &[u32],
    _: &[Keysym],
  ) {
    self.plat.activate_keyboard(keyboard);
    self.plat.serial = serial;
    let Some(idx) = self.plat.window_index_for_surface(surface) else {
      return;
    };
    self.plat.focused_window = idx;
    self.plat.windows[idx].focused = true;
    let id = self.plat.windows[idx].id;
    self.app.on_focus(&mut self.plat, id, true);
  }

  fn leave(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlKeyboard,
    surface: &WlSurface,
    _: u32,
  ) {
    let Some(idx) = self.plat.window_index_for_surface(surface) else {
      return;
    };
    self.plat.windows[idx].focused = false;
    let id = self.plat.windows[idx].id;
    self.app.on_focus(&mut self.plat, id, false);
  }

  fn press_key(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    keyboard: &WlKeyboard,
    serial: u32,
    event: KeyEvent,
  ) {
    self.plat.activate_keyboard(keyboard);
    self.plat.serial = serial;
    if let Some(id) = self.plat.focused_id() {
      let mods = self.plat.modifiers_for(keyboard);
      self.app.on_key(&mut self.plat, id, &event, mods);
    }
  }

  fn repeat_key(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlKeyboard,
    _: u32,
    _: KeyEvent,
  ) {
    // Repeats are delivered through the get_keyboard_with_repeat callback.
  }

  fn release_key(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    keyboard: &WlKeyboard,
    _: u32,
    event: KeyEvent,
  ) {
    if let Some(id) = self.plat.focused_id() {
      let mods = self.plat.modifiers_for(keyboard);
      self.app.on_key_release(&mut self.plat, id, &event, mods);
    }
  }

  fn update_modifiers(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    keyboard: &WlKeyboard,
    _: u32,
    modifiers: Modifiers,
    _: RawModifiers,
    _: u32,
  ) {
    self.plat.set_modifiers_for(keyboard, modifiers);
  }

  fn update_repeat_info(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlKeyboard,
    _: RepeatInfo,
  ) {
  }
}

impl PointerHandler for WaylandState {
  fn pointer_frame(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    pointer: &WlPointer,
    events: &[PointerEvent],
  ) {
    self.plat.activate_pointer(pointer);
    for event in events {
      // Pointer focus is independent of keyboard focus; route each event to the
      // window its own surface names.
      let Some(id) = self.plat.window_id_for_surface(&event.surface) else {
        continue;
      };
      let (x, y) = event.position;
      let neutral = match &event.kind {
        PointerEventKind::Enter { serial } => {
          // Record the enter serial so cursor-shape requests for this window
          // carry a serial the compositor will accept.
          if let Some(i) = self.plat.window_index_for_surface(&event.surface) {
            self.plat.windows[i].pointer_enter_serial = *serial;
          }
          WEvent::Enter {
            x,
            y,
            serial: *serial,
          }
        },
        PointerEventKind::Leave { .. } => WEvent::Leave,
        PointerEventKind::Motion { .. } => WEvent::Motion { x, y },
        PointerEventKind::Press {
          button: b, serial, ..
        } => {
          self.plat.serial = *serial;
          WEvent::Press {
            x,
            y,
            button: button(*b),
            serial: *serial,
          }
        },
        PointerEventKind::Release { button: b, .. } => {
          WEvent::Release {
            x,
            y,
            button: button(*b),
          }
        },
        PointerEventKind::Axis {
          horizontal,
          vertical,
          ..
        } => {
          // Wheel notches arrive as value120 (÷120) or legacy discrete steps;
          // touchpads send absolute pixels. Hand the app a logical-pixel delta
          // plus whether it was a discrete notch.
          // `discrete` drives the app's vertical dy classification, so judge it
          // from the vertical axis alone - a horizontal notch must not make a
          // smooth vertical delta look like a wheel step.
          let discrete = vertical.value120 != 0 || vertical.discrete != 0;
          let dy = if vertical.value120 != 0 {
            f64::from(vertical.value120) / 120.0
          } else if vertical.discrete != 0 {
            f64::from(vertical.discrete)
          } else {
            vertical.absolute
          };
          let dx = if horizontal.value120 != 0 {
            f64::from(horizontal.value120) / 120.0
          } else if horizontal.discrete != 0 {
            f64::from(horizontal.discrete)
          } else {
            horizontal.absolute
          };
          WEvent::Axis {
            x,
            y,
            scroll: Scroll { dx, dy, discrete },
          }
        },
      };
      let mods = self.plat.modifiers();
      self.app.on_pointer(&mut self.plat, id, neutral, mods);
    }
  }
}

impl TouchHandler for WaylandState {
  fn down(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlTouch,
    _serial: u32,
    _time: u32,
    surface: WlSurface,
    id: i32,
    position: (f64, f64),
  ) {
    if let Some(wid) = self.plat.window_id_for_surface(&surface) {
      // Latch this touch point to the window it started on.
      self.plat.touch_focus.insert(id, wid);
      self.app.on_touch(&mut self.plat, wid, TouchEvent::Down {
        id,
        x: position.0,
        y: position.1,
      });
    }
  }

  fn up(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlTouch,
    _serial: u32,
    _time: u32,
    id: i32,
  ) {
    if let Some(wid) = self.plat.touch_focus.remove(&id) {
      self
        .app
        .on_touch(&mut self.plat, wid, TouchEvent::Up { id });
    }
  }

  fn motion(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlTouch,
    _time: u32,
    id: i32,
    position: (f64, f64),
  ) {
    if let Some(wid) = self.plat.touch_focus.get(&id).copied() {
      self.app.on_touch(&mut self.plat, wid, TouchEvent::Motion {
        id,
        x: position.0,
        y: position.1,
      });
    }
  }

  fn shape(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlTouch,
    _: i32,
    _: f64,
    _: f64,
  ) {
  }

  fn orientation(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlTouch,
    _: i32,
    _: f64,
  ) {
  }

  fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlTouch) {
    // Cancel every window a live touch point was latched to.
    let wids: Vec<WindowId> =
      self.plat.touch_focus.drain().map(|(_, w)| w).collect();
    for wid in wids {
      self.app.on_touch(&mut self.plat, wid, TouchEvent::Cancel);
    }
  }
}

impl OutputHandler for WaylandState {
  fn output_state(&mut self) -> &mut OutputState {
    &mut self.plat.output_state
  }

  fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {
  }
  fn update_output(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: WlOutput,
  ) {
  }
  fn output_destroyed(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: WlOutput,
  ) {
  }
}

impl ProvidesRegistryState for WaylandState {
  fn registry(&mut self) -> &mut RegistryState {
    &mut self.plat.registry_state
  }
  registry_handlers![OutputState, SeatState];
}

impl DataDeviceHandler for WaylandState {
  fn enter(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataDevice,
    _: f64,
    _: f64,
    _: &WlSurface,
  ) {
  }
  fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
  }
  fn motion(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataDevice,
    _: f64,
    _: f64,
  ) {
  }
  fn selection(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataDevice,
  ) {
  }
  fn drop_performed(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataDevice,
  ) {
  }
}

impl DataOfferHandler for WaylandState {
  fn source_actions(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &mut DragOffer,
    _: DndAction,
  ) {
  }
  fn selected_action(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &mut DragOffer,
    _: DndAction,
  ) {
  }
}

impl DataSourceHandler for WaylandState {
  fn accept_mime(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataSource,
    _: Option<String>,
  ) {
  }

  fn send_request(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    source: &WlDataSource,
    _mime: String,
    fd: WritePipe,
  ) {
    if self
      .plat
      .copy_source
      .as_ref()
      .is_some_and(|s| s.inner() == source)
    {
      self.plat.serve_selection(
        self.app.clipboard_text(false).unwrap_or_default(),
        fd,
      );
    }
  }

  fn cancelled(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    source: &WlDataSource,
  ) {
    if self
      .plat
      .copy_source
      .as_ref()
      .is_some_and(|s| s.inner() == source)
    {
      self.plat.copy_source = None;
    }
  }

  fn dnd_dropped(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataSource,
  ) {
  }
  fn dnd_finished(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataSource,
  ) {
  }
  fn action(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &WlDataSource,
    _: DndAction,
  ) {
  }
}

impl PrimarySelectionDeviceHandler for WaylandState {
  fn selection(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    _: &ZwpPrimarySelectionDeviceV1,
  ) {
  }
}

impl PrimarySelectionSourceHandler for WaylandState {
  fn send_request(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    source: &ZwpPrimarySelectionSourceV1,
    _mime: String,
    fd: WritePipe,
  ) {
    if self
      .plat
      .primary_source
      .as_ref()
      .is_some_and(|s| s.inner() == source)
    {
      self
        .plat
        .serve_selection(self.app.clipboard_text(true).unwrap_or_default(), fd);
    }
  }

  fn cancelled(
    &mut self,
    _: &Connection,
    _: &QueueHandle<Self>,
    source: &ZwpPrimarySelectionSourceV1,
  ) {
    if self
      .plat
      .primary_source
      .as_ref()
      .is_some_and(|s| s.inner() == source)
    {
      self.plat.primary_source = None;
    }
  }
}

impl Dispatch<WpFractionalScaleV1, WindowId> for WaylandState {
  fn event(
    state: &mut Self,
    _: &WpFractionalScaleV1,
    event: wp_fractional_scale_v1::Event,
    id: &WindowId,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    // The object carries its window's id, so a scale change is applied to the
    // surface it actually names rather than to whatever holds keyboard focus.
    if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
      && let Some(idx) = state.plat.window_index(*id)
    {
      state.plat.windows[idx].scale120 = scale;
      state.app.on_scale(&mut state.plat, *id, scale);
    }
  }
}

impl ActivationHandler for WaylandState {
  type RequestUdata = ();

  fn new_token(&mut self, token: String, _: &RequestData<()>) {
    if let (Some(activation), true) = (
      self.plat.activation.as_ref(),
      self.plat.focused_window < self.plat.windows.len(),
    ) {
      let surface = self.plat.windows[self.plat.focused_window]
        .window
        .wl_surface();
      activation.activate::<Self>(surface, token);
    }
  }
}

// text-input-v3 batches preedit/commit between `enter` and `done`; translate
// each event to a neutral `ImeEvent` for the app to apply.
impl Dispatch<ZwpTextInputV3, ()> for WaylandState {
  fn event(
    state: &mut Self,
    ti: &ZwpTextInputV3,
    event: zwp_text_input_v3::Event,
    (): &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    use zwp_text_input_v3::Event;
    let Some(id) = state.plat.focused_id() else {
      return;
    };
    match event {
      Event::Enter { .. } => {
        ti.enable();
        ti.set_content_type(ContentHint::None, ContentPurpose::Terminal);
        ti.commit();
        state.app.on_ime(&mut state.plat, id, ImeEvent::Enable);
      },
      Event::Leave { .. } => {
        ti.disable();
        ti.commit();
        state.app.on_ime(&mut state.plat, id, ImeEvent::Disable);
      },
      Event::PreeditString { text, .. } => {
        state.app.on_ime(
          &mut state.plat,
          id,
          ImeEvent::Preedit(text.unwrap_or_default()),
        );
      },
      Event::CommitString { text } => {
        state.app.on_ime(
          &mut state.plat,
          id,
          ImeEvent::Commit(text.unwrap_or_default()),
        );
      },
      Event::Done { .. } => {
        state.app.on_ime(&mut state.plat, id, ImeEvent::Done);
      },
      Event::DeleteSurroundingText {
        before_length,
        after_length,
      } => {
        state
          .app
          .on_ime(&mut state.plat, id, ImeEvent::DeleteSurrounding {
            before: before_length,
            after:  after_length,
          });
      },
      _ => {},
    }
  }
}
