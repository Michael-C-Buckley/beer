use super::*;

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        factor: i32,
    ) {
        // Integer fallback for compositors without fractional-scale-v1; ignored
        // when the fractional-scale object drives the scale instead.
        if self.fractional_scale.is_none() {
            self.set_scale((factor.max(1) as u32) * 120);
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

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // The compositor is ready for another frame; `flush` will repaint if the
        // grid has changed since the last present.
        self.frame_pending = false;
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
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        if let (Some(w), Some(h)) = configure.new_size {
            self.width = w.get();
            self.height = h.get();
            if let Some(vp) = &self.viewport {
                vp.set_destination(self.width.max(1) as i32, self.height.max(1) as i32);
            }
        }
        self.focused = configure.is_activated();
        if self.session.is_none() {
            self.spawn_session();
        } else {
            self.resize_grid();
        }
        self.needs_draw = true;
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
        let data_device = Some(self.data_device_manager.get_data_device(qh, &seat));
        let primary_device = self
            .primary_manager
            .as_ref()
            .map(|m| m.get_selection_device(qh, &seat));
        let i = self.seat_index(&seat);
        self.seats[i].data_device = data_device;
        self.seats[i].primary_device = primary_device;
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        let i = self.seat_index(&seat);
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
        _: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.activate_keyboard(keyboard);
        self.serial = serial;
        self.focused = true;
        self.report_focus(true);
        self.needs_draw = true;
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.focused = false;
        self.report_focus(false);
        self.needs_draw = true;
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
        _: KeyEvent,
    ) {
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

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        self.activate_pointer(pointer);
        let cell_h = f64::from(self.renderer.metrics().height);
        for event in events {
            match &event.kind {
                PointerEventKind::Enter { serial } => {
                    self.pointer_pos = event.position;
                    self.pointer_enter_serial = *serial;
                    self.update_hover(pointer);
                    self.pointer_drag();
                }
                PointerEventKind::Motion { .. } => {
                    self.pointer_pos = event.position;
                    if self.try_report_motion() {
                        continue;
                    }
                    if !self.selecting {
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
                    self.pointer_pos = event.position;
                    if let Some(code) = button_code(*button)
                        && self.try_report_button(code, true)
                    {
                        self.pressed_button = Some(code);
                        continue;
                    }
                    match *button {
                        BTN_LEFT => {
                            self.press_cell = self.cell_at(self.pointer_pos.0, self.pointer_pos.1);
                            self.pointer_press(*time);
                        }
                        BTN_MIDDLE => self.paste_primary(),
                        _ => {}
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    self.pointer_pos = event.position;
                    let code = button_code(*button);
                    if let Some(code) = code
                        && self.try_report_button(code, false)
                    {
                        if self.pressed_button == Some(code) {
                            self.pressed_button = None;
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
                    if self.mouse_reporting() {
                        let code = if up { 64 } else { 65 };
                        for _ in 0..lines.clamp(1, 8) {
                            self.try_report_button(code, true);
                        }
                        continue;
                    }
                    // On the alternate screen there is no scrollback to move, so
                    // (when enabled) translate the wheel into cursor-key presses
                    // for apps that did not request mouse reporting.
                    let alt = self
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
                    if let Some(session) = self.session.as_mut() {
                        session.term.scroll_view(delta);
                        self.needs_draw = true;
                    }
                }
                _ => {}
            }
        }
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
            state.set_scale(scale);
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

impl ActivationHandler for App {
    type RequestData = RequestData;

    fn new_token(&mut self, token: String, _: &RequestData) {
        // The compositor granted an activation token; use it to draw attention.
        if let Some(activation) = self.activation.as_ref() {
            activation.activate::<App>(self.window.wl_surface(), token);
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
                state.ime_set_cursor_rect(ti);
                ti.commit();
            }
            Event::Leave { .. } => {
                ti.disable();
                ti.commit();
                state.preedit.clear();
                state.ime_preedit_pending.clear();
                state.ime_commit_pending.clear();
                state.needs_draw = true;
            }
            Event::PreeditString { text, .. } => {
                state.ime_preedit_pending = text.unwrap_or_default();
            }
            Event::CommitString { text } => {
                state.ime_commit_pending.push_str(&text.unwrap_or_default());
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
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_data_device!(App);
delegate_primary_selection!(App);
delegate_registry!(App);
delegate_activation!(App);
