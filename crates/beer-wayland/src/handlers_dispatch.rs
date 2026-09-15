//! Dispatch plumbing for protocol objects that sctk does not wrap directly.

use smithay_client_toolkit::{
  activation::RequestData,
  compositor::{FrameCallbackData, SurfaceData},
  data_device_manager::{
    data_device::DataDeviceData,
    data_offer::DataOfferData,
    data_source::DataSourceData,
  },
  delegate_registry,
  globals::GlobalData,
  output::OutputData,
  primary_selection::{
    device::PrimarySelectionDeviceData,
    offer::PrimarySelectionOfferData,
  },
  reexports::protocols::{
    wp::{
      cursor_shape::v1::client::{
        wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
        wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
      },
      primary_selection::zv1::client::{
        zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
        zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
        zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
      },
    },
    xdg::{
      activation::v1::client::{
        xdg_activation_token_v1::XdgActivationTokenV1,
        xdg_activation_v1::XdgActivationV1,
      },
      decoration::zv1::client::{
        zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
      },
      dialog::v1::client::xdg_wm_dialog_v1::XdgWmDialogV1,
      shell::client::{
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
        xdg_wm_base::XdgWmBase,
      },
      xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::ZxdgOutputV1,
      },
    },
  },
  seat::{
    SeatData as SctkSeatData,
    keyboard::KeyboardData,
    pointer::PointerData,
    touch::TouchData,
  },
  shell::xdg::window::WindowData,
};
use wayland_client::{
  Connection,
  Dispatch,
  Proxy,
  QueueHandle,
  protocol::{
    wl_callback::WlCallback,
    wl_compositor::WlCompositor,
    wl_data_device::WlDataDevice,
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::WlDataOffer,
    wl_data_source::WlDataSource,
    wl_keyboard::WlKeyboard,
    wl_output::WlOutput,
    wl_pointer::WlPointer,
    wl_seat::WlSeat,
    wl_shm::WlShm,
    wl_surface::WlSurface,
    wl_touch::WlTouch,
  },
};
use wayland_protocols::wp::{
  content_type::v1::client::{
    wp_content_type_manager_v1::WpContentTypeManagerV1,
    wp_content_type_v1::{self, WpContentTypeV1},
  },
  fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
  idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
    zwp_idle_inhibitor_v1::{self, ZwpIdleInhibitorV1},
  },
  text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3,
  viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use crate::state::WaylandState;

macro_rules! noop_dispatch {
  ($($iface:ty => $ev:ty),+ $(,)?) => {$(
    impl Dispatch<$iface, ()> for WaylandState {
      fn event(_: &mut Self, _: &$iface, _: $ev, (): &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
  )+};
}

noop_dispatch! {
  WpFractionalScaleManagerV1 => <WpFractionalScaleManagerV1 as Proxy>::Event,
  WpViewporter => <WpViewporter as Proxy>::Event,
  WpViewport => <WpViewport as Proxy>::Event,
  ZwpIdleInhibitManagerV1 => <ZwpIdleInhibitManagerV1 as Proxy>::Event,
  ZwpIdleInhibitorV1 => zwp_idle_inhibitor_v1::Event,
  WpContentTypeManagerV1 => <WpContentTypeManagerV1 as Proxy>::Event,
  WpContentTypeV1 => wp_content_type_v1::Event,
  ZwpTextInputManagerV3 => <ZwpTextInputManagerV3 as Proxy>::Event,
}

// sctk 0.21's blanket `delegate_dispatch2!` cycles on `TouchData`, so forward
// each concrete (interface, user-data) pair to its `Dispatch2` impl by hand.
macro_rules! forward_dispatch {
  ($($iface:ty => $data:ty),+ $(,)?) => {$(
    impl ::wayland_client::Dispatch<$iface, $data> for WaylandState {
      fn event(
        state: &mut Self,
        proxy: &$iface,
        event: <$iface as ::wayland_client::Proxy>::Event,
        data: &$data,
        conn: &::wayland_client::Connection,
        qh: &::wayland_client::QueueHandle<Self>,
      ) {
        <$data as ::smithay_client_toolkit::dispatch2::Dispatch2<$iface, WaylandState>>::event(
          data, state, proxy, event, conn, qh,
        );
      }
      fn event_created_child(
        opcode: u16,
        qh: &::wayland_client::QueueHandle<Self>,
      ) -> ::std::sync::Arc<dyn ::wayland_client::backend::ObjectData> {
        <$data as ::smithay_client_toolkit::dispatch2::Dispatch2<$iface, WaylandState>>
          ::event_created_child(opcode, qh)
      }
    }
  )+};
}

forward_dispatch! {
  WlCompositor => GlobalData,
  WlSurface => SurfaceData<()>,
  WlCallback => FrameCallbackData,
  WlOutput => OutputData,
  ZxdgOutputManagerV1 => GlobalData,
  ZxdgOutputV1 => OutputData,
  WlShm => GlobalData,
  WlSeat => SctkSeatData,
  WlKeyboard => KeyboardData<WaylandState, ()>,
  WlPointer => PointerData<()>,
  WlTouch => TouchData<()>,
  XdgWmBase => GlobalData,
  XdgSurface => WindowData,
  XdgToplevel => WindowData,
  XdgWmDialogV1 => GlobalData,
  ZxdgDecorationManagerV1 => GlobalData,
  ZxdgToplevelDecorationV1 => WindowData,
  WlDataDeviceManager => GlobalData,
  WlDataDevice => DataDeviceData,
  WlDataSource => DataSourceData<()>,
  WlDataOffer => DataOfferData,
  ZwpPrimarySelectionDeviceManagerV1 => GlobalData,
  ZwpPrimarySelectionDeviceV1 => PrimarySelectionDeviceData,
  ZwpPrimarySelectionSourceV1 => GlobalData,
  ZwpPrimarySelectionOfferV1 => PrimarySelectionOfferData,
  WpCursorShapeManagerV1 => GlobalData,
  WpCursorShapeDeviceV1 => GlobalData,
  XdgActivationV1 => GlobalData,
  XdgActivationTokenV1 => RequestData<()>,
}

delegate_registry!(WaylandState);
