//! Backend state: the process-wide Wayland client state and per-window platform
//! state. This holds only platform objects; terminal state (sessions,
//! selection, search/url/IME mode) lives in the [`beer_window::App`] the
//! backend drives.
//!
//! [`WaylandState`] is the calloop loop data and the sctk `Dispatch` target. It
//! splits into `app` (the terminal, behind `dyn App`) and `plat` (all platform
//! objects). Keeping them as separate fields lets a handler call
//! `self.app.on_*(&mut self.plat, …)` - two disjoint borrows - where `plat`
//! serves as the [`beer_window::WindowCtx`]. `dyn App` keeps the backend
//! non-generic; the app trait is object-safe.

use std::collections::HashMap;

use beer_window::{App, WindowId};
use calloop::{LoopHandle, RegistrationToken};
use smithay_client_toolkit::{
  activation::ActivationState,
  compositor::CompositorState,
  data_device_manager::{
    DataDeviceManagerState,
    data_device::DataDevice,
    data_source::CopyPasteSource,
  },
  output::OutputState,
  primary_selection::{
    PrimarySelectionManagerState,
    device::PrimarySelectionDevice,
    selection::PrimarySelectionSource,
  },
  reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
  registry::RegistryState,
  seat::{SeatState, keyboard::Modifiers, pointer::cursor_shape::CursorShapeManager},
  shell::xdg::{XdgShell, window::Window as XdgWindow},
  shm::{
    Shm,
    slot::{Buffer, SlotPool},
  },
};
use wayland_client::{
  QueueHandle,
  protocol::{
    wl_keyboard::WlKeyboard,
    wl_pointer::WlPointer,
    wl_seat::WlSeat,
    wl_surface::WlSurface,
    wl_touch::WlTouch,
  },
};
use wayland_protocols::wp::{
  content_type::v1::client::{
    wp_content_type_manager_v1::WpContentTypeManagerV1,
    wp_content_type_v1::WpContentTypeV1,
  },
  fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::WpFractionalScaleV1,
  },
  idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
    zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
  },
  text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::ZwpTextInputV3,
  },
  viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

/// Buffers kept for double/triple buffering before waiting for a release.
pub const MAX_BUFFERS: usize = 3;

/// One shm buffer in a window's ring, tagged with a stable id so the app can
/// key its per-buffer damage snapshots to it across reuse.
#[derive(Debug)]
pub struct FrameBuf {
  pub buffer: Buffer,
  pub id:     u64,
}

/// Input devices for one seat. Several seats can drive the windows; the most
/// recently used one owns clipboard/primary claims.
#[derive(Debug)]
pub struct SeatData {
  pub seat:                WlSeat,
  pub keyboard:            Option<WlKeyboard>,
  /// Modifier state of this seat's keyboard, kept per-seat so two keyboards do
  /// not clobber one another's shift/ctrl/alt state.
  pub modifiers:           Modifiers,
  pub pointer:             Option<WlPointer>,
  pub cursor_shape_device: Option<WpCursorShapeDeviceV1>,
  pub data_device:         Option<DataDevice>,
  pub primary_device:      Option<PrimarySelectionDevice>,
  pub text_input:          Option<ZwpTextInputV3>,
  pub touch:               Option<WlTouch>,
}

/// Per-window platform state: the surface, its shm buffer ring, and the
/// presentation/scale bookkeeping for one toplevel.
#[derive(Debug)]
#[expect(
  clippy::struct_excessive_bools,
  reason = "independent per-window compositor/paint flags"
)]
pub struct PlatformWindow {
  pub id:                   WindowId,
  pub pool:                 SlotPool,
  pub window:               XdgWindow,
  pub idle_inhibitor:       Option<ZwpIdleInhibitorV1>,
  /// Kept alive so the surface content-type hint persists; never read back.
  #[expect(dead_code, reason = "held to preserve the content-type hint")]
  pub content_type:         Option<WpContentTypeV1>,
  pub viewport:             Option<WpViewport>,
  pub fractional_scale:     Option<WpFractionalScaleV1>,
  /// Compositor's preferred scale in 120ths (120 = 1.0, 180 = 1.5).
  pub scale120:             u32,
  /// Wayland `app_id`, kept for the activation (urgency) request.
  pub app_id:               String,
  pub title:                Option<String>,
  pub fullscreen:           bool,
  pub width:                u32,
  pub height:               u32,
  /// Double/triple-buffer ring.
  pub frames:               Vec<FrameBuf>,
  /// Monotonic id for the next ring buffer, so the app can key snapshots.
  pub next_buf_id:          u64,
  /// Pixel size the `frames` buffers were allocated for.
  pub buf_dims:             (u32, u32),
  /// A `wl_surface.frame` callback is in flight; defer drawing until it fires.
  pub frame_pending:        bool,
  /// The app asked for a repaint; paint on the next opportunity.
  pub wants_draw:           bool,
  /// Serial of the last pointer enter, reused to update the cursor shape.
  pub pointer_enter_serial: u32,
  pub focused:              bool,
}

/// All platform objects and windows, and the [`beer_window::WindowCtx`] the app
/// acts through. Kept separate from `app` on [`WaylandState`] so handlers can
/// borrow both at once.
pub struct Platform {
  pub registry_state:       RegistryState,
  pub output_state:         OutputState,
  pub seat_state:           SeatState,
  pub shm:                  Shm,
  pub loop_handle:          LoopHandle<'static, WaylandState>,
  pub qh:                   QueueHandle<WaylandState>,
  pub compositor:           CompositorState,
  pub xdg_shell:            XdgShell,
  pub viewporter:           Option<WpViewporter>,
  pub fractional_manager:   Option<WpFractionalScaleManagerV1>,
  pub content_type_manager: Option<WpContentTypeManagerV1>,
  pub data_device_manager:  DataDeviceManagerState,
  pub primary_manager:      Option<PrimarySelectionManagerState>,
  pub cursor_shape_manager: Option<CursorShapeManager>,
  pub text_input_manager:   Option<ZwpTextInputManagerV3>,
  pub activation:           Option<ActivationState>,
  pub idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
  pub seats:                Vec<SeatData>,
  pub active_seat:          usize,
  pub copy_source:          Option<CopyPasteSource>,
  pub primary_source:       Option<PrimarySelectionSource>,
  /// Advertised clipboard/primary text, taken from the app on claim.
  pub clipboard:            String,
  pub primary_clip:         String,
  /// Most recent input serial, used to claim selections.
  pub serial:               u32,
  pub windows:              Vec<PlatformWindow>,
  pub focused_window:       usize,
  /// Window a touch point is latched to, keyed by touch id, so a gesture stays
  /// with the surface it started on regardless of keyboard focus.
  pub touch_focus:          HashMap<i32, WindowId>,
  pub next_window_id:       u64,
  /// App-registered event sources (pty fd, IPC sockets, timers) keyed by the
  /// token the app addresses them with, so they can be removed.
  pub sources:              HashMap<u64, RegistrationToken>,
  pub exit:                 bool,
  pub exit_code:            u8,
}

/// The calloop loop data and sctk `Dispatch` target: the terminal application
/// plus all platform state. Handlers call `self.app.on_*(&mut self.plat, …)`.
pub struct WaylandState {
  pub app:  Box<dyn App>,
  pub plat: Platform,
}

impl Platform {
  /// Index of the window with `id`, if present.
  pub fn window_index(&self, id: WindowId) -> Option<usize> {
    self.windows.iter().position(|w| w.id == id)
  }

  /// Index of the window backing `surface`, if any.
  pub fn window_index_for_surface(&self, surface: &WlSurface) -> Option<usize> {
    use smithay_client_toolkit::shell::WaylandSurface;
    self
      .windows
      .iter()
      .position(|w| w.window.wl_surface() == surface)
  }

  /// The id of the window backing `surface`, if any.
  pub fn window_id_for_surface(&self, surface: &WlSurface) -> Option<WindowId> {
    self
      .window_index_for_surface(surface)
      .map(|i| self.windows[i].id)
  }

  /// The focused window's id, if the index is valid.
  pub fn focused_id(&self) -> Option<WindowId> {
    self.windows.get(self.focused_window).map(|w| w.id)
  }

  /// Next window id, monotonically increasing and never reused.
  pub const fn alloc_window_id(&mut self) -> WindowId {
    let id = WindowId(self.next_window_id);
    self.next_window_id += 1;
    id
  }

  /// Find (or create) the per-seat entry for `seat`, returning its index.
  pub fn seat_index(&mut self, seat: &WlSeat) -> usize {
    if let Some(i) = self.seats.iter().position(|s| &s.seat == seat) {
      return i;
    }
    self.seats.push(SeatData {
      seat:                seat.clone(),
      keyboard:            None,
      modifiers:           Modifiers::default(),
      pointer:             None,
      cursor_shape_device: None,
      data_device:         None,
      primary_device:      None,
      text_input:          None,
      touch:               None,
    });
    self.seats.len() - 1
  }

  /// Clipboard/primary devices are seat-scoped; attach them once per seat.
  pub fn ensure_clipboard_devices(
    &mut self,
    qh: &QueueHandle<WaylandState>,
    seat: &WlSeat,
    i: usize,
  ) {
    if self.seats[i].data_device.is_none() {
      let dd = self.data_device_manager.get_data_device(qh, seat);
      self.seats[i].data_device = Some(dd);
    }
    if self.seats[i].primary_device.is_none()
      && let Some(m) = self.primary_manager.as_ref()
    {
      self.seats[i].primary_device = Some(m.get_selection_device(qh, seat));
    }
  }

  /// Mark the seat owning `keyboard` as active for clipboard ownership.
  pub fn activate_keyboard(&mut self, keyboard: &WlKeyboard) {
    if let Some(i) = self
      .seats
      .iter()
      .position(|s| s.keyboard.as_ref() == Some(keyboard))
    {
      self.active_seat = i;
    }
  }

  /// Modifier state of the active seat.
  pub fn modifiers(&self) -> Modifiers {
    self
      .seats
      .get(self.active_seat)
      .map(|s| s.modifiers)
      .unwrap_or_default()
  }

  /// Modifier state of the seat owning `keyboard`.
  pub fn modifiers_for(&self, keyboard: &WlKeyboard) -> Modifiers {
    self
      .seats
      .iter()
      .find(|s| s.keyboard.as_ref() == Some(keyboard))
      .map(|s| s.modifiers)
      .unwrap_or_default()
  }

  /// Record the modifier state for the seat owning `keyboard`.
  pub fn set_modifiers_for(
    &mut self,
    keyboard: &WlKeyboard,
    modifiers: Modifiers,
  ) {
    if let Some(s) = self
      .seats
      .iter_mut()
      .find(|s| s.keyboard.as_ref() == Some(keyboard))
    {
      s.modifiers = modifiers;
    }
  }

  /// Mark the seat owning `pointer` as active for clipboard ownership.
  pub fn activate_pointer(&mut self, pointer: &WlPointer) {
    if let Some(i) = self
      .seats
      .iter()
      .position(|s| s.pointer.as_ref() == Some(pointer))
    {
      self.active_seat = i;
    }
  }
}
