//! Application construction and window-coordinate helpers.

use std::{collections::HashMap, path::PathBuf};

use beer_window::{Modifiers, WindowId};

use super::{App, TOKEN_BASE, WinState, font_options, grid_size};
use crate::{
  bindings::Bindings,
  config::Config,
  font::Fonts,
  ipc,
  render::Renderer,
};
impl App {
  /// Build the terminal application from a loaded config.
  pub fn new(
    config: Config,
    config_paths: Vec<PathBuf>,
    server: bool,
    initial: ipc::OpenRequest,
  ) -> anyhow::Result<Self> {
    use anyhow::Context as _;
    // Validate the font up front by building the scale-1.0 renderer; other
    // output scales get their own renderer lazily (never re-rasterized on a
    // window switch).
    let fonts =
      Fonts::new(&font_options(&config.main, config.main.font_size, 120))
        .context("load font")?;
    let mut renderer = Renderer::new(fonts);
    renderer.set_padding(config.main.pad_x, config.main.pad_y);
    renderer.set_alpha_blending(config.colors.alpha_blending);
    let mut renderers = HashMap::new();
    renderers.insert(120, renderer);
    let bindings = Bindings::from_config(
      &config.key_bindings,
      &config.text_bindings,
      &config.mouse_bindings,
    );
    let font_size = config.main.font_size;
    let resident = server && config.main.server_resident;
    Ok(Self {
      renderers,
      config,
      config_paths,
      bindings,
      font_size,
      blink_on: true,
      rapid_on: true,
      blink_armed: false,
      rapid_armed: false,
      anim_armed: false,
      ipc_sweep_armed: false,
      modifiers: Modifiers::default(),
      windows: Vec::new(),
      focused: 0,
      clipboard: String::new(),
      primary_clip: String::new(),
      exit_code: 0,
      server,
      resident,
      next_token: TOKEN_BASE,
      next_window: 1,
      initial,
      ipc_clients: HashMap::new(),
      ipc_listener: None,
      ipc_socket: None,
    })
  }

  pub(super) const fn alloc_token(&mut self) -> u64 {
    let t = self.next_token;
    self.next_token += 1;
    t
  }

  pub(super) fn win_index(&self, id: WindowId) -> Option<usize> {
    self.windows.iter().position(|w| w.id == id)
  }

  #[expect(
    clippy::unused_self,
    clippy::cast_possible_truncation,
    reason = "method for call-site symmetry; the 120ths quotient fits u32"
  )]
  pub(super) fn to_phys(&self, w: &WinState, v: u32) -> u32 {
    ((u64::from(v) * u64::from(w.scale120) + 60) / 120) as u32
  }

  #[expect(clippy::unused_self, reason = "method for call-site symmetry")]
  pub(super) fn to_phys_f(&self, w: &WinState, v: f64) -> f64 {
    v * f64::from(w.scale120) / 120.0
  }

  pub(super) fn phys_dims(&self, w: &WinState) -> (u32, u32) {
    (
      self.to_phys(w, w.width).max(1),
      self.to_phys(w, w.height).max(1),
    )
  }

  pub(super) fn grid_dims(&self, w: &WinState) -> (u16, u16) {
    let (pw, ph) = self.phys_dims(w);
    let m = w.metrics;
    let pad = (
      self.to_phys(w, self.config.main.pad_x),
      self.to_phys(w, self.config.main.pad_y),
    );
    grid_size(m, pw, ph, pad)
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel→cell mapping is clamped to the grid bounds"
  )]
  pub(super) fn cell_at(
    &self,
    w: &WinState,
    px: f64,
    py: f64,
  ) -> Option<(usize, usize)> {
    let session = w.session.as_ref()?;
    let m = w.metrics;
    let (pad_x, pad_y) = (
      f64::from(self.to_phys(w, self.config.main.pad_x)),
      f64::from(self.to_phys(w, self.config.main.pad_y)),
    );
    let (px, py) = (self.to_phys_f(w, px), self.to_phys_f(w, py));
    let grid = session.term.grid();
    let col = ((px - pad_x).max(0.0) as usize / m.width as usize)
      .min(grid.cols().saturating_sub(1));
    let vrow = ((py - pad_y).max(0.0) as usize / m.height as usize)
      .min(grid.rows().saturating_sub(1));
    Some((grid.view_to_abs(vrow), col))
  }
}
