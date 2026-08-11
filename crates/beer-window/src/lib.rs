//! Platform-neutral windowing contract for the beer terminal.
//!
//! A backend (e.g. `beer-wayland`) owns the platform connection, event loop,
//! surfaces, and shm buffers. It drives an [`App`] by translating native input
//! and lifecycle events into `App` method calls, passing a [`WindowCtx`] the
//! app uses to act on windows (present a frame, set the title, own the
//! clipboard, open/close toplevels, watch file descriptors and arm timers).
//!
//! The app never names a platform (Wayland) type: input is delivered as neutral
//! [`PointerEvent`]/[`TouchEvent`]/[`ImeEvent`] values, and keyboard input
//! reuses the xkb keysym vocabulary (`KeyEvent`/`Modifiers`) that the
//! `beer-protocols` encoders already consume - that vocabulary is an input
//! abstraction, not a Wayland protocol object, so a future x11/macOS backend
//! maps its native keys onto the same keysyms.
//!
//! Backend selection is a `beer`-level concern: it calls the chosen backend's
//! `run(app, ...)` entry directly (cfg-selected once a second backend exists),
//! so no `Backend` trait is needed here - the portable seam is `App` +
//! `WindowCtx` + the event vocabulary below.

use std::os::fd::RawFd;

pub use beer_protocols::key::KeyKind;
pub use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};

/// Stable per-window identifier, assigned by the backend and never reused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WindowId(pub u64);

/// A pointer button, normalized across platforms. `Other` carries a raw code
/// for bindings that target extra buttons.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointerButton {
  /// Primary button.
  Left,
  /// Middle button.
  Middle,
  /// Secondary button.
  Right,
  /// A platform-specific button code.
  Other(u16),
}

/// The pointer icon shown over a window. `Text` is the terminal I-beam,
/// `Pointer` the hand shown over hyperlinks, `Default` the compositor arrow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorIcon {
  /// The compositor's default pointer.
  Default,
  /// A text insertion pointer.
  Text,
  /// A pointer used for hyperlinks.
  Pointer,
}

/// A scroll step in surface-logical pixels. `discrete` marks a wheel notch (as
/// opposed to smooth touchpad scrolling), so the app can apply its multiplier.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Scroll {
  /// Horizontal scroll distance in logical pixels.
  pub dx:       f64,
  /// Vertical scroll distance in logical pixels.
  pub dy:       f64,
  /// Whether the event represents a discrete wheel step.
  pub discrete: bool,
}

/// A pointer event in surface-logical pixel coordinates.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PointerEvent {
  /// The pointer entered the surface.
  Enter {
    /// Horizontal surface coordinate.
    x:      f64,
    /// Vertical surface coordinate.
    y:      f64,
    /// Compositor serial for the enter event.
    serial: u32,
  },
  /// The pointer left the surface.
  Leave,
  /// The pointer moved within the surface.
  Motion {
    /// Horizontal surface coordinate.
    x: f64,
    /// Vertical surface coordinate.
    y: f64,
  },
  /// A pointer button was pressed.
  Press {
    /// Horizontal surface coordinate.
    x:      f64,
    /// Vertical surface coordinate.
    y:      f64,
    /// Pressed button.
    button: PointerButton,
    /// Compositor serial for the press event.
    serial: u32,
  },
  /// A pointer button was released.
  Release {
    /// Horizontal surface coordinate.
    x:      f64,
    /// Vertical surface coordinate.
    y:      f64,
    /// Released button.
    button: PointerButton,
  },
  /// The pointer's scroll axis changed.
  Axis {
    /// Horizontal surface coordinate.
    x:      f64,
    /// Vertical surface coordinate.
    y:      f64,
    /// Scroll delta.
    scroll: Scroll,
  },
}

/// A single-finger touch event; the backend filters multi-finger gestures out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TouchEvent {
  /// A touch point was placed on the surface.
  Down {
    /// Backend-assigned touch-point identifier.
    id: i32,
    /// Horizontal surface coordinate.
    x:  f64,
    /// Vertical surface coordinate.
    y:  f64,
  },
  /// A touch point moved.
  Motion {
    /// Backend-assigned touch-point identifier.
    id: i32,
    /// Horizontal surface coordinate.
    x:  f64,
    /// Vertical surface coordinate.
    y:  f64,
  },
  /// A touch point was lifted.
  Up {
    /// Backend-assigned touch-point identifier.
    id: i32,
  },
  /// The compositor cancelled the touch sequence.
  Cancel,
}

/// IME (text-input) events forwarded from the compositor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImeEvent {
  /// Begin receiving text-input events.
  Enable,
  /// Stop receiving text-input events.
  Disable,
  /// Uncommitted preedit text to show inline at the cursor.
  Preedit(String),
  /// Committed text to send to the shell.
  Commit(String),
  /// End of one text-input transaction; the app applies the batched preedit/
  /// commit and reports its cursor rectangle.
  Done,
}

/// A physical-pixel frame buffer handed to the app to paint. `fresh` marks a
/// newly allocated buffer whose margins are uninitialised (paint in full).
#[derive(Debug)]
pub struct Frame<'a> {
  /// Mutable physical-pixel storage to paint.
  pub pixels: &'a mut [u8],
  /// Opaque id of the backing buffer, stable across reuse, so the app can key
  /// per-buffer damage snapshots to it.
  pub id:     u64,
  /// Whether the buffer contents must be painted in full.
  pub fresh:  bool,
}

/// Platform actions the app performs on windows, implemented by the backend and
/// handed to every [`App`] callback.
pub trait WindowCtx {
  /// Logical (post-scale) surface size in pixels, `None` before first
  /// configure.
  fn size(&self, id: WindowId) -> Option<(u32, u32)>;
  /// Compositor scale in 120ths (120 = 1.0, 180 = 1.5).
  fn scale120(&self, id: WindowId) -> u32;
  /// Set the window title.
  fn set_title(&mut self, id: WindowId, title: &str);
  /// Toggle fullscreen state.
  fn set_fullscreen(&mut self, id: WindowId, on: bool);
  /// Request a logical window size in pixels. Used before the first present to
  /// apply the configured initial size when the compositor leaves sizing to
  /// the client.
  fn request_size(&mut self, id: WindowId, width: u32, height: u32);
  /// Set the pointer icon shown over the window.
  fn set_cursor(&mut self, id: WindowId, icon: CursorIcon);
  /// Hold an idle inhibitor while the window is focused, per user opt-in.
  fn set_idle_inhibit(&mut self, id: WindowId, inhibit: bool);
  /// Request the compositor's attention on an urgent bell while unfocused.
  fn request_attention(&mut self, id: WindowId);

  /// Acquire a physical-pixel buffer to paint, or `None` when every buffer is
  /// still held by the compositor (a later release re-drives [`App::render`]).
  fn acquire(
    &mut self,
    id: WindowId,
    phys_w: u32,
    phys_h: u32,
  ) -> Option<Frame<'_>>;
  /// Present the last acquired buffer of `id`: attach, present at the logical
  /// size via a viewport, damage the `dirty` rows (or fully when `full`),
  /// commit, and request a frame callback. `cell_h`/`pad_y` place row damage.
  fn present(
    &mut self,
    id: WindowId,
    buffer: u64,
    dirty: &[u32],
    cell_h: u32,
    pad_y: u32,
    full: bool,
  );
  /// Ask the backend to call [`App::render`] for `id` when it is next able to
  /// paint (no frame callback in flight).
  fn request_redraw(&mut self, id: WindowId);

  /// Report the IME candidate-window anchor rectangle, in logical pixels.
  fn set_ime_cursor(&mut self, id: WindowId, x: i32, y: i32, w: i32, h: i32);

  /// Take ownership of the clipboard / primary selection, advertising `text`;
  /// the backend serves reads by calling [`App::clipboard_text`].
  fn claim_clipboard(&mut self, text: String);
  /// Take ownership of the primary selection, advertising `text`.
  fn claim_primary(&mut self, text: String);
  /// Request the clipboard/primary contents; delivered later via
  /// [`App::on_paste`].
  fn request_paste(&mut self, id: WindowId, primary: bool);

  /// Create a new toplevel and return its id; the app owns the session behind
  /// it and spawns the shell on the first configure.
  fn open_window(&mut self) -> WindowId;
  /// Request that a window be closed.
  fn close_window(&mut self, id: WindowId);

  /// Watch `fd` for readability; fires [`App::on_readable`] with `token` until
  /// [`WindowCtx::unwatch`] is called. Used for the pty master and IPC sockets.
  fn watch_readable(&mut self, fd: RawFd, token: u64);
  /// Stop watching a previously registered file descriptor token.
  fn unwatch(&mut self, token: u64);
  /// Arm a one-shot timer firing [`App::on_timer`] with `token` after `millis`.
  fn arm_timer(&mut self, token: u64, millis: u64);
  /// Cancel a previously armed timer.
  fn cancel_timer(&mut self, token: u64);

  /// Exit the loop with process status `code` once control returns.
  fn exit(&mut self, code: u8);
}

/// The terminal application, driven by a backend. Every method receives a
/// [`WindowCtx`] for platform actions; the app owns terminal state (sessions,
/// selection, search/url/IME mode) and never touches a platform object.
pub trait App {
  /// Called once when the loop is ready: arm global timers (blink/animation),
  /// begin serving the IPC socket in server mode, and open the initial window.
  fn start(&mut self, ctx: &mut dyn WindowCtx);

  /// Apply a compositor resize/configure event.
  fn on_configure(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    width: u32,
    height: u32,
    activated: bool,
    resizing: bool,
  );
  /// Apply a compositor scale change.
  fn on_scale(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, scale120: u32);
  /// Apply a focus change.
  fn on_focus(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, focused: bool);
  /// Handle a request to close a window.
  fn on_close(&mut self, ctx: &mut dyn WindowCtx, id: WindowId);

  /// A key was pressed or auto-repeated (the compositor's repeat timer delivers
  /// both through this path). The app distinguishes press from repeat via its
  /// own held-key tracking, mapping to [`KeyKind`] for the encoders.
  fn on_key(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: &KeyEvent,
    mods: Modifiers,
  );
  /// A key was released (used only by the kitty keyboard protocol's release
  /// reporting).
  fn on_key_release(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: &KeyEvent,
    mods: Modifiers,
  );
  /// Handle an IME event.
  fn on_ime(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, event: ImeEvent);

  /// Handle a pointer event and its modifier state.
  fn on_pointer(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: PointerEvent,
    mods: Modifiers,
  );
  /// Handle a touch event.
  fn on_touch(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: TouchEvent,
  );

  /// Pasted bytes arrived for an earlier [`WindowCtx::request_paste`].
  fn on_paste(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    data: &[u8],
    primary: bool,
  );
  /// The text the app currently owns for the clipboard/primary, served to
  /// other clients' paste requests.
  fn clipboard_text(&self, primary: bool) -> Option<String>;

  /// A watched fd (pty master, IPC listener/client) became readable.
  fn on_readable(&mut self, ctx: &mut dyn WindowCtx, token: u64);
  /// A timer armed via [`WindowCtx::arm_timer`] (or a recurring global one)
  /// fired.
  fn on_timer(&mut self, ctx: &mut dyn WindowCtx, token: u64);
  /// The config-reload signal (SIGUSR1) fired.
  fn on_reload(&mut self, ctx: &mut dyn WindowCtx);

  /// Repaint `id` if its displayed state changed since the last present. Called
  /// after [`WindowCtx::request_redraw`] once the backend can paint.
  fn render(&mut self, ctx: &mut dyn WindowCtx, id: WindowId);
}
