use super::*;
use crate::bindings::MouseButton;

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        // Integer fallback for compositors without fractional-scale-v1; ignored
        // when the fractional-scale object drives the scale instead.
        let idx = self.window_index(surface).unwrap_or(self.focused_window);
        if self.windows[idx].fractional_scale.is_none() {
            self.set_scale(idx, (factor.max(1) as u32) * 120);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        // The compositor is ready for another frame; `flush` will repaint if the
        // grid has changed since the last present.
        let idx = self.window_index(surface).unwrap_or(self.focused_window);
        self.windows[idx].frame_pending = false;
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &XdgWindow) {
        // Single window: closing it exits the process. Route to the matching
        // window first so per-window teardown can generalize later.
        let _ = self.window_index(window.wl_surface());
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &XdgWindow,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let idx = self
            .window_index(window.wl_surface())
            .unwrap_or(self.focused_window);
        if let (Some(w), Some(h)) = configure.new_size {
            self.windows[idx].width = w.get();
            self.windows[idx].height = h.get();
            if let Some(vp) = &self.windows[idx].viewport {
                let (ww, hh) = (self.windows[idx].width, self.windows[idx].height);
                vp.set_destination(ww.max(1) as i32, hh.max(1) as i32);
            }
        }
        self.windows[idx].focused = configure.is_activated();
        self.sync_idle_inhibit();
        if self.windows[idx].session.is_none() {
            self.spawn_session(idx);
        } else {
            self.resize_grid(idx);
        }
        self.windows[idx].needs_draw = true;
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        // Clipboard/primary devices are seat-scoped, not capability-scoped.
        let i = self.seat_index(&seat);
        self.ensure_clipboard_devices(qh, &seat, i);
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        let i = self.seat_index(&seat);
        // A pre-existing seat may reach us via its capabilities without a
        // `new_seat` call, so make sure the clipboard devices are attached.
        self.ensure_clipboard_devices(qh, &seat, i);
        if capability == Capability::Keyboard && self.seats[i].keyboard.is_none() {
            // get_keyboard_with_repeat drives key repeat off a calloop timer and
            // delivers each repeat through the callback.
            let loop_handle = self.loop_handle.clone();
            let keyboard = self.seat_state.get_keyboard_with_repeat(
                qh,
                &seat,
                None,
                loop_handle,
                Box::new(|app: &mut App, _kbd, event| app.handle_key(&event)),
            );
            match keyboard {
                Ok(keyboard) => self.seats[i].keyboard = Some(keyboard),
                Err(err) => tracing::warn!("get keyboard: {err}"),
            }
            if self.seats[i].text_input.is_none()
                && let Some(mgr) = self.text_input_manager.as_ref()
            {
                self.seats[i].text_input = Some(mgr.get_text_input(&seat, qh, ()));
            }
        }
        if capability == Capability::Pointer && self.seats[i].pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => {
                    self.seats[i].cursor_shape_device = self
                        .cursor_shape_manager
                        .as_ref()
                        .map(|m| m.get_shape_device(&pointer, qh));
                    self.seats[i].pointer = Some(pointer);
                }
                Err(err) => tracing::warn!("get pointer: {err}"),
            }
        }
        if capability == Capability::Touch && self.seats[i].touch.is_none() {
            match self.seat_state.get_touch(qh, &seat) {
                Ok(touch) => self.seats[i].touch = Some(touch),
                Err(err) => tracing::warn!("get touch: {err}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        let Some(s) = self.seats.iter_mut().find(|s| s.seat == seat) else {
            return;
        };
        match capability {
            Capability::Keyboard => {
                if let Some(keyboard) = s.keyboard.take() {
                    keyboard.release();
                }
            }
            Capability::Pointer => {
                s.cursor_shape_device = None;
                if let Some(pointer) = s.pointer.take() {
                    pointer.release();
                }
            }
            Capability::Touch => {
                if let Some(touch) = s.touch.take() {
                    touch.release();
                }
                self.windows[self.focused_window].touch_scroll = None;
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seats.retain(|s| s.seat != seat);
        self.active_seat = self.active_seat.min(self.seats.len().saturating_sub(1));
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.activate_keyboard(keyboard);
        self.serial = serial;
        let idx = self.window_index(surface).unwrap_or(self.focused_window);
        self.focused_window = idx;
        self.windows[idx].focused = true;
        self.sync_idle_inhibit();
        self.report_focus(true);
        self.windows[idx].needs_draw = true;
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        let idx = self.window_index(surface).unwrap_or(self.focused_window);
        self.windows[idx].focused = false;
        self.sync_idle_inhibit();
        // Drop held-key state so a key released while unfocused can't leak a
        // stale kitty release event later.
        self.windows[idx].keys_down.clear();
        self.report_focus(false);
        self.windows[idx].needs_draw = true;
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.activate_keyboard(keyboard);
        self.serial = serial;
        self.handle_key(&event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
        // Repeats are delivered through the get_keyboard_with_repeat callback;
        // this non-calloop hook is unused.
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key_release(&event);
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: RepeatInfo,
    ) {
    }
}

/// Parse a configured cursor-style name into a [`CursorShape`].
pub(super) fn cursor_shape_from(style: Option<&str>) -> Option<CursorShape> {
    match style? {
        "block" => Some(CursorShape::Block),
        "beam" | "bar" => Some(CursorShape::Beam),
        "underline" => Some(CursorShape::Underline),
        other => {
            tracing::warn!("unknown cursor style {other:?}");
            None
        }
    }
}

/// Generate `n` distinct keyboard hint labels (a, b, …, z, aa, ab, …), all the
/// same length so prefix matching is unambiguous.
pub(super) fn hint_labels(n: usize) -> Vec<String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return Vec::new();
    }
    let (mut width, mut capacity) = (1usize, 26usize);
    while capacity < n {
        width += 1;
        capacity *= 26;
    }
    (0..n)
        .map(|i| {
            let mut idx = i;
            let mut chars = vec![b'a'; width];
            for slot in chars.iter_mut().rev() {
                *slot = ALPHABET[idx % 26];
                idx /= 26;
            }
            String::from_utf8(chars).expect("ascii labels are valid utf-8")
        })
        .collect()
}

/// Map a Wayland button code to the terminal mouse base code, if reportable.
fn button_code(button: u32) -> Option<u8> {
    match button {
        BTN_LEFT => Some(0),
        BTN_MIDDLE => Some(1),
        BTN_RIGHT => Some(2),
        _ => None,
    }
}

/// Map a Wayland button code to a bindable [`MouseButton`].
fn mouse_button(button: u32) -> Option<MouseButton> {
    match button {
        BTN_LEFT => Some(MouseButton::Left),
        BTN_MIDDLE => Some(MouseButton::Middle),
        BTN_RIGHT => Some(MouseButton::Right),
        _ => None,
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        self.activate_pointer(pointer);
        let idx = self.focused_window;
        let cell_h = f64::from(self.renderer.metrics().height);
        for event in events {
            match &event.kind {
                PointerEventKind::Enter { serial } => {
                    self.windows[idx].pointer_pos = event.position;
                    self.windows[idx].pointer_enter_serial = *serial;
                    self.update_hover(pointer);
                    self.pointer_drag();
                }
                PointerEventKind::Motion { .. } => {
                    self.windows[idx].pointer_pos = event.position;
                    if self.try_report_motion() {
                        continue;
                    }
                    if !self.windows[idx].selecting {
                        self.update_hover(pointer);
                    }
                    self.pointer_drag();
                }
                PointerEventKind::Press {
                    button,
                    serial,
                    time,
                    ..
                } => {
                    self.serial = *serial;
                    self.windows[idx].pointer_pos = event.position;
                    if let Some(code) = button_code(*button)
                        && self.try_report_button(code, true)
                    {
                        self.windows[idx].pressed_button = Some(code);
                        continue;
                    }
                    // A configured `[mouse-bindings]` action (Middle defaults to
                    // primary paste) fires before the built-in left-drag select.
                    if let Some(mb) = mouse_button(*button)
                        && let Some(action) = self.bindings.mouse_action(mb, self.modifiers)
                    {
                        self.dispatch_action(action);
                        continue;
                    }
                    if *button == BTN_LEFT {
                        let (px, py) = self.windows[idx].pointer_pos;
                        let cell = self.cell_at(idx, px, py);
                        self.windows[idx].press_cell = cell;
                        self.pointer_press(*time);
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    self.windows[idx].pointer_pos = event.position;
                    let code = button_code(*button);
                    if let Some(code) = code
                        && self.try_report_button(code, false)
                    {
                        if self.windows[idx].pressed_button == Some(code) {
                            self.windows[idx].pressed_button = None;
                        }
                        continue;
                    }
                    if *button == BTN_LEFT {
                        self.maybe_open_clicked_link();
                        self.pointer_release(qh);
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    // Wheel notches arrive as value120 (÷120) or legacy discrete
                    // steps, ~3 lines each; touchpads send pixels per cell height.
                    let (raw, scale) = if vertical.value120 != 0 {
                        (f64::from(vertical.value120) / 120.0, 3.0)
                    } else if vertical.discrete != 0 {
                        (f64::from(vertical.discrete), 3.0)
                    } else if cell_h > 0.0 {
                        (vertical.absolute / cell_h, 1.0)
                    } else {
                        continue;
                    };
                    if raw == 0.0 {
                        continue;
                    }
                    let mult = self.config.mouse.scroll_multiplier.max(0.0);
                    let lines = (raw.abs() * scale * mult).ceil().max(1.0) as isize;
                    let up = raw < 0.0;
                    // Reporting apps get wheel buttons (64 up / 65 down) as
                    // presses, one per line, capped so a flick cannot flood.
                    if self.mouse_reporting(idx) {
                        let code = if up { 64 } else { 65 };
                        for _ in 0..lines.clamp(1, 8) {
                            self.try_report_button(code, true);
                        }
                        continue;
                    }
                    // On the alternate screen there is no scrollback to move, so
                    // (when enabled) translate the wheel into cursor-key presses
                    // for apps that did not request mouse reporting.
                    let alt = self.windows[idx]
                        .session
                        .as_ref()
                        .is_some_and(|s| s.term.grid().alt_active());
                    if alt && self.config.mouse.alternate_scroll {
                        self.alternate_scroll(up, lines.clamp(1, 8));
                        continue;
                    }
                    // Positive axis = scroll down (toward live); the viewport
                    // scrolls the opposite way (negative offset delta).
                    let delta = if up { lines } else { -lines };
                    let win = &mut self.windows[idx];
                    if let Some(session) = win.session.as_mut() {
                        session.term.scroll_view(delta);
                        win.needs_draw = true;
                    }
                }
                _ => {}
            }
        }
    }
}

// Touch drives one-finger drag-to-scroll of the scrollback viewport. Taps and
// multi-finger gestures are ignored; only the first touch point is tracked.
impl TouchHandler for App {
    fn down(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _serial: u32,
        _time: u32,
        surface: wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        let idx = self.window_index(&surface).unwrap_or(self.focused_window);
        if self.windows[idx].touch_scroll.is_none() {
            self.windows[idx].touch_scroll = Some(TouchScroll {
                id,
                last_y: position.1,
                acc: 0.0,
            });
        }
    }

    fn up(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _serial: u32,
        _time: u32,
        id: i32,
    ) {
        let idx = self.focused_window;
        if self.windows[idx]
            .touch_scroll
            .as_ref()
            .is_some_and(|t| t.id == id)
        {
            self.windows[idx].touch_scroll = None;
        }
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _time: u32,
        id: i32,
        position: (f64, f64),
    ) {
        let idx = self.focused_window;
        let cell_h = self.renderer.metrics().height as f64;
        let win = &mut self.windows[idx];
        let Some(touch) = win.touch_scroll.as_mut().filter(|t| t.id == id) else {
            return;
        };
        touch.acc += position.1 - touch.last_y;
        touch.last_y = position.1;
        // Dragging down (positive delta) reveals older lines, matching the
        // viewport's "positive scrolls back" convention.
        let lines = (touch.acc / cell_h) as isize;
        if lines != 0 {
            touch.acc -= lines as f64 * cell_h;
            if let Some(session) = win.session.as_mut() {
                session.term.scroll_view(lines);
                win.needs_draw = true;
            }
        }
    }

    fn shape(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: i32,
        _: f64,
        _: f64,
    ) {
    }

    fn orientation(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: i32,
        _: f64,
    ) {
    }

    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch) {
        self.windows[self.focused_window].touch_scroll = None;
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

/// Serve the held clipboard text when a paste target requests it.
fn serve(text: &str, fd: WritePipe) {
    let mut file = File::from(OwnedFd::from(fd));
    let _ = file.write_all(text.as_bytes());
}

impl DataDeviceHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for App {
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

impl DataSourceHandler for App {
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
            .copy_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            serve(&self.clipboard, fd);
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if self
            .copy_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            self.copy_source = None;
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl PrimarySelectionDeviceHandler for App {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl PrimarySelectionSourceHandler for App {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        _mime: String,
        fd: WritePipe,
    ) {
        if self
            .primary_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            serve(&self.primary_clip, fd);
        }
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        if self
            .primary_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            self.primary_source = None;
        }
    }
}

// Fractional-scale and viewporter are not wrapped by sctk, so dispatch them by
// hand. Only the fractional-scale object carries an event we act on.
impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let idx = state.focused_window;
            state.set_scale(idx, scale);
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewporter, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// idle-inhibit and content-type are likewise raw protocol objects; none of them
// emit events we act on.
impl Dispatch<ZwpIdleInhibitManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpIdleInhibitManagerV1,
        _: <ZwpIdleInhibitManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpIdleInhibitorV1,
        _: zwp_idle_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpContentTypeManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpContentTypeManagerV1,
        _: <WpContentTypeManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpContentTypeV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpContentTypeV1,
        _: wp_content_type_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl ActivationHandler for App {
    type RequestData = RequestData;

    fn new_token(&mut self, token: String, _: &RequestData) {
        // The compositor granted an activation token; use it to draw attention.
        if let Some(activation) = self.activation.as_ref() {
            activation
                .activate::<App>(self.windows[self.focused_window].window.wl_surface(), token);
        }
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpTextInputManagerV3,
        _: <ZwpTextInputManagerV3 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// text-input-v3 batches preedit/commit between `enter` and `done`; we apply the
// accumulated transaction on `done` and re-enable on focus enter.
impl Dispatch<ZwpTextInputV3, ()> for App {
    fn event(
        state: &mut Self,
        ti: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_text_input_v3::Event;
        match event {
            Event::Enter { .. } => {
                ti.enable();
                ti.set_content_type(ContentHint::None, ContentPurpose::Terminal);
                let idx = state.focused_window;
                state.ime_set_cursor_rect(idx, ti);
                ti.commit();
            }
            Event::Leave { .. } => {
                ti.disable();
                ti.commit();
                let win = &mut state.windows[state.focused_window];
                win.preedit.clear();
                win.ime_preedit_pending.clear();
                win.ime_commit_pending.clear();
                win.needs_draw = true;
            }
            Event::PreeditString { text, .. } => {
                state.windows[state.focused_window].ime_preedit_pending = text.unwrap_or_default();
            }
            Event::CommitString { text } => {
                state.windows[state.focused_window]
                    .ime_commit_pending
                    .push_str(&text.unwrap_or_default());
            }
            Event::Done { .. } => state.ime_done(ti),
            // We do not expose surrounding text, so nothing to delete.
            Event::DeleteSurroundingText { .. } => {}
            _ => {}
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_touch!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_data_device!(App);
delegate_primary_selection!(App);
delegate_registry!(App);
delegate_activation!(App);
