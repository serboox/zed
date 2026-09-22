use std::{
    cell::{RefCell, RefMut},
    hash::Hash,
    os::fd::{AsRawFd, BorrowedFd},
    path::PathBuf,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use ashpd::WindowIdentifier;
use calloop::{
    EventLoop, LoopHandle,
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use collections::HashMap;
use filedescriptor::Pipe;
use gpui_util::ResultExt as _;
use http_client::Url;
use smallvec::SmallVec;
use wayland_backend::client::ObjectId;
use wayland_backend::protocol::WEnum;
use wayland_client::event_created_child;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_offer::WlDataOffer;
use wayland_client::protocol::wl_pointer::AxisSource;
use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_output, wl_region,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_pinch_v1, zwp_pointer_gestures_v1,
};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::{
    self, ZwpPrimarySelectionOfferV1,
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1, zwp_primary_selection_device_v1,
    zwp_primary_selection_source_v1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    ContentHint, ContentPurpose,
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols::xdg::system_bell::v1::client::xdg_system_bell_v1;
use wayland_protocols::{
    wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1},
    xdg::dialog::v1::client::xdg_wm_dialog_v1::{self, XdgWmDialogV1},
};
use wayland_protocols::{
    wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
    xdg::dialog::v1::client::xdg_dialog_v1::XdgDialogV1,
};
use wayland_protocols_plasma::blur::client::{org_kde_kwin_blur, org_kde_kwin_blur_manager};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon::xkb::ffi::XKB_KEYMAP_FORMAT_TEXT_V1;
use xkbcommon::xkb::{self, KEYMAP_COMPILE_NO_FLAGS, Keycode};

use super::{
    display::WaylandDisplay,
    window::{ImeInput, WaylandWindowStatePtr},
};

use crate::linux::{
    DOUBLE_CLICK_INTERVAL, GNOME_COPIED_FILES_MIME_TYPE, LinuxClient, LinuxCommon,
    LinuxKeyboardLayout, PIPE_READ_TIMEOUT, SCROLL_LINES, capslock_from_xkb,
    cursor_style_to_icon_names, external_paths, get_xkb_compose_state, is_within_click_distance,
    keystroke_from_xkb, keystroke_underlying_dead_key, modifiers_from_xkb, open_uri_internal,
    read_fd_with_timeout, reveal_path_internal,
    wayland::{
        clipboard::{
            Clipboard, DataOffer, FILE_LIST_MIME_TYPE, TEXT_MIME_TYPES, formats_a_file_drag_offers,
        },
        cursor::Cursor,
        serial::{SerialKind, SerialTracker},
        to_shape,
        window::WaylandWindow,
    },
    xdg_desktop_portal::{Event as XDPEvent, XDPEventSource},
};
use gpui::{
    AnyWindowHandle, Bounds, Capslock, CursorStyle, DevicePixels, DisplayId, FileDropEvent,
    ForegroundExecutor, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection,
    Pixels, PlatformDisplay, PlatformInput, PlatformKeyboardLayout, PlatformWindow, Point,
    ScrollDelta, ScrollWheelEvent, SharedString, Size, TouchPhase, WindowButtonLayout, WindowKind,
    WindowParams, point, profiler, px, size,
};
use gpui_wgpu::{CompositorGpuHint, GpuContext};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};

/// Used to convert evdev scancode to xkb scancode
const MIN_KEYCODE: u32 = 8;

const UNKNOWN_KEYBOARD_LAYOUT_NAME: SharedString = SharedString::new_static("unknown");
const XDG_ACTIVATION_TOKEN_ENV_VAR: &str = "XDG_ACTIVATION_TOKEN";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImeCursorRectangle {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl From<Bounds<Pixels>> for ImeCursorRectangle {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: bounds.origin.x.as_f32() as i32,
            y: bounds.origin.y.as_f32() as i32,
            width: bounds.size.width.as_f32() as i32,
            height: bounds.size.height.as_f32() as i32,
        }
    }
}

trait ImeCursorRectangleSink {
    fn set_ime_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32);
    fn commit_ime_state(&self);
}

impl ImeCursorRectangleSink for zwp_text_input_v3::ZwpTextInputV3 {
    fn set_ime_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
        self.set_cursor_rectangle(x, y, width, height);
    }

    fn commit_ime_state(&self) {
        self.commit();
    }
}

fn set_ime_cursor_rectangle(
    text_input: &impl ImeCursorRectangleSink,
    cursor_rectangle: ImeCursorRectangle,
) {
    text_input.set_ime_cursor_rectangle(
        cursor_rectangle.x,
        cursor_rectangle.y,
        cursor_rectangle.width,
        cursor_rectangle.height,
    );
}

fn update_ime_cursor_rectangle(
    text_input: &impl ImeCursorRectangleSink,
    last_ime_cursor_rectangle: &mut Option<ImeCursorRectangle>,
    bounds: Bounds<Pixels>,
) {
    let cursor_rectangle = ImeCursorRectangle::from(bounds);
    if *last_ime_cursor_rectangle == Some(cursor_rectangle) {
        return;
    }

    *last_ime_cursor_rectangle = Some(cursor_rectangle);
    set_ime_cursor_rectangle(text_input, cursor_rectangle);
    text_input.commit_ime_state();
}

fn set_ime_cursor_rectangle_after_done(
    text_input: &impl ImeCursorRectangleSink,
    last_ime_cursor_rectangle: &mut Option<ImeCursorRectangle>,
    bounds: Bounds<Pixels>,
    should_commit: bool,
) {
    if should_commit {
        update_ime_cursor_rectangle(text_input, last_ime_cursor_rectangle, bounds);
    } else {
        set_ime_cursor_rectangle(text_input, ImeCursorRectangle::from(bounds));
    }
}

fn take_startup_activation_token_from_environment() -> Option<String> {
    let startup_activation_token = std::env::var(XDG_ACTIVATION_TOKEN_ENV_VAR)
        .ok()
        .filter(|token| !token.is_empty());
    // The token must be removed from the environment so it isn't inherited by child
    // processes we spawn, per the xdg-activation spec: https://wayland.app/protocols/xdg-activation-v1
    // SAFETY: This runs during Wayland platform initialization before GPUI starts
    // concurrent environment access or spawning child processes.
    unsafe { std::env::remove_var(XDG_ACTIVATION_TOKEN_ENV_VAR) };
    startup_activation_token
}

#[derive(Clone)]
pub struct Globals {
    pub qh: QueueHandle<WaylandClientStatePtr>,
    pub activation: Option<xdg_activation_v1::XdgActivationV1>,
    pub compositor: wl_compositor::WlCompositor,
    pub cursor_shape_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    pub data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    pub primary_selection_manager:
        Option<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,
    pub wm_base: xdg_wm_base::XdgWmBase,
    pub shm: wl_shm::WlShm,
    pub seat: wl_seat::WlSeat,
    pub viewporter: Option<wp_viewporter::WpViewporter>,
    pub fractional_scale_manager:
        Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    pub decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    pub layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    pub blur_manager: Option<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager>,
    pub text_input_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    pub gesture_manager: Option<zwp_pointer_gestures_v1::ZwpPointerGesturesV1>,
    pub dialog: Option<xdg_wm_dialog_v1::XdgWmDialogV1>,
    pub system_bell: Option<xdg_system_bell_v1::XdgSystemBellV1>,
    pub executor: ForegroundExecutor,
}

impl Globals {
    fn new(
        globals: GlobalList,
        executor: ForegroundExecutor,
        qh: QueueHandle<WaylandClientStatePtr>,
        seat: wl_seat::WlSeat,
    ) -> Self {
        let dialog_v = XdgWmDialogV1::interface().version;
        Globals {
            activation: globals.bind(&qh, 1..=1, ()).ok(),
            compositor: globals
                .bind(
                    &qh,
                    wl_surface::REQ_SET_BUFFER_SCALE_SINCE
                        ..=wl_surface::EVT_PREFERRED_BUFFER_SCALE_SINCE,
                    (),
                )
                .unwrap(),
            cursor_shape_manager: globals.bind(&qh, 1..=1, ()).ok(),
            data_device_manager: globals
                .bind(
                    &qh,
                    WL_DATA_DEVICE_MANAGER_VERSION..=WL_DATA_DEVICE_MANAGER_VERSION,
                    (),
                )
                .ok(),
            primary_selection_manager: globals.bind(&qh, 1..=1, ()).ok(),
            shm: globals.bind(&qh, 1..=1, ()).unwrap(),
            seat,
            wm_base: globals.bind(&qh, 1..=5, ()).unwrap(),
            viewporter: globals.bind(&qh, 1..=1, ()).ok(),
            fractional_scale_manager: globals.bind(&qh, 1..=1, ()).ok(),
            decoration_manager: globals.bind(&qh, 1..=1, ()).ok(),
            layer_shell: globals.bind(&qh, 1..=5, ()).ok(),
            blur_manager: globals.bind(&qh, 1..=1, ()).ok(),
            text_input_manager: globals.bind(&qh, 1..=1, ()).ok(),
            gesture_manager: globals.bind(&qh, 1..=3, ()).ok(),
            dialog: globals.bind(&qh, dialog_v..=dialog_v, ()).ok(),
            system_bell: globals.bind(&qh, 1..=1, ()).ok(),
            executor,
            qh,
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub struct InProgressOutput {
    name: Option<String>,
    scale: Option<i32>,
    position: Option<Point<DevicePixels>>,
    size: Option<Size<DevicePixels>>,
    subpixel: Option<wl_output::Subpixel>,
}

impl InProgressOutput {
    fn complete(&self) -> Option<Output> {
        if let Some((position, size)) = self.position.zip(self.size) {
            let scale = self.scale.unwrap_or(1);
            Some(Output {
                name: self.name.clone(),
                scale,
                bounds: Bounds::new(position, size),
                subpixel: self.subpixel,
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Output {
    pub name: Option<String>,
    pub scale: i32,
    pub bounds: Bounds<DevicePixels>,
    pub subpixel: Option<wl_output::Subpixel>,
}

pub(crate) struct WaylandClientState {
    serial_tracker: SerialTracker,
    globals: Globals,
    pub gpu_context: GpuContext,
    pub compositor_gpu: Option<CompositorGpuHint>,
    wl_seat: wl_seat::WlSeat, // TODO: Multi seat support
    wl_pointer: Option<wl_pointer::WlPointer>,
    pinch_gesture: Option<zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1>,
    pinch_scale: f32,
    wl_keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_shape_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    data_device: Option<wl_data_device::WlDataDevice>,
    /// The files of a drag this client started and the desktop has not finished
    /// with yet. Kept here rather than on the data source, because the source
    /// only ever hands back the mime type it is being asked for.
    dragged_files: Vec<PathBuf>,
    /// What the desktop settled on doing with that drag. A move is only carried
    /// out once the drop is finished, and it is this client that carries it out:
    /// whoever took the files has copied them, and removing the originals is
    /// what turns that copy into a move.
    dragged_action: DndAction,
    /// The picture the desktop carries under the pointer, and what backs it. It
    /// has to outlive the drag, so it is held here and let go when the drag ends.
    drag_icon: Option<DragIcon>,
    primary_selection: Option<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,
    text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    pre_edit_text: Option<String>,
    ime_pre_edit: Option<String>,
    composing: bool,
    last_ime_cursor_rectangle: Option<ImeCursorRectangle>,
    // Surface to Window mapping
    windows: HashMap<ObjectId, WaylandWindowStatePtr>,
    // Output to scale mapping
    outputs: HashMap<ObjectId, Output>,
    in_progress_outputs: HashMap<ObjectId, InProgressOutput>,
    wl_outputs: HashMap<ObjectId, wl_output::WlOutput>,
    keyboard_layout: LinuxKeyboardLayout,
    keymap_state: Option<xkb::State>,
    compose_state: Option<xkb::compose::State>,
    drag: DragState,
    click: ClickState,
    repeat: KeyRepeat,
    pub modifiers: Modifiers,
    pub capslock: Capslock,
    axis_source: AxisSource,
    pub mouse_location: Option<Point<Pixels>>,
    continuous_scroll_delta: Option<Point<Pixels>>,
    discrete_scroll_delta: Option<Point<f32>>,
    vertical_modifier: f32,
    horizontal_modifier: f32,
    scroll_event_received: bool,
    enter_token: Option<()>,
    button_pressed: Option<MouseButton>,
    mouse_focused_window: Option<WaylandWindowStatePtr>,
    keyboard_focused_window: Option<WaylandWindowStatePtr>,
    loop_handle: LoopHandle<'static, WaylandClientStatePtr>,
    cursor_style: Option<CursorStyle>,
    cursor_hidden_window: Option<WaylandWindowStatePtr>,
    clipboard: Clipboard,
    data_offers: Vec<DataOffer<WlDataOffer>>,
    primary_data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
    cursor: Cursor,
    pending_activation: Option<PendingActivation>,
    startup_activation_token: Option<String>,
    event_loop: Option<EventLoop<'static, WaylandClientStatePtr>>,
    pub common: LinuxCommon,
    ime_enabled: Option<bool>,
}

/// How wide and tall the drag picture is, in the buffer's own pixels. It is
/// shown at half that, so it stays sharp on a screen that doubles everything
/// and is merely smooth on one that does not.
const DRAG_ICON_SIZE: i32 = 128;
const DRAG_ICON_SCALE: i32 = 2;

/// The picture the desktop carries under the pointer: a page, and a second one
/// behind it when more than one file is being carried.
///
/// Drawn here rather than taken from an icon theme because this is the only
/// place in the client that draws anything at all, and one page is what every
/// desktop draws for a file. `wl_shm` wants premultiplied ARGB in native order,
/// which for the colours below -- all fully opaque -- is the colour as written.
fn drag_icon_pixels(count: usize) -> Vec<u8> {
    const INK: u32 = 0xff30_3642;
    const PAPER: u32 = 0xffe8_ecf2;
    const BEHIND: u32 = 0xffb9_c1cd;
    const FOLD: u32 = 0xffc3_cad4;

    let side = DRAG_ICON_SIZE as usize;
    let mut pixels = vec![0u32; side * side];
    let mut page = |left: i32, top: i32, right: i32, bottom: i32, fill: u32, fold: bool| {
        for y in top.max(0)..bottom.min(DRAG_ICON_SIZE) {
            for x in left.max(0)..right.min(DRAG_ICON_SIZE) {
                let edge = x < left + 2 || x >= right - 2 || y < top + 2 || y >= bottom - 2;
                // The top right corner is folded away, and the fold itself is
                // what tells a page from a plain rectangle.
                let corner = right - x + (y - top);
                let folded = fold && corner > (right - left) && corner <= (right - left) + 26;
                let at = y as usize * side + x as usize;
                pixels[at] = match (folded, edge) {
                    (true, _) if corner <= (right - left) + 4 => 0,
                    (true, _) => FOLD,
                    (_, true) => INK,
                    _ => fill,
                };
            }
        }
    };
    if count > 1 {
        page(20, 26, 96, 116, BEHIND, false);
    }
    page(32, 12, 108, 102, PAPER, true);

    pixels
        .into_iter()
        .flat_map(|pixel| pixel.to_ne_bytes())
        .collect()
}

/// An anonymous file holding `bytes`, for the compositor to read the drag
/// picture out of. It lives as long as the descriptor does.
fn a_file_holding(bytes: &[u8]) -> Option<std::fs::File> {
    use std::io::{Seek, Write};
    use std::os::fd::FromRawFd;

    let name = c"zed-drag-icon";
    // Safety: the name is a valid C string and the descriptor returned is ours.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if fd < 0 {
        return None;
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes).ok()?;
    file.flush().ok()?;
    file.rewind().ok()?;
    Some(file)
}

/// What a drag that has just ended leaves behind: everything it carried when
/// the desktop settled on moving it, and nothing at all otherwise.
///
/// A move is only half done when the drop lands -- whoever took the files has
/// copied them -- and letting go of these is what makes it a move. Anything
/// else and the files stay exactly where they were.
/// The one action a drag puts on offer for what it means.
///
/// One and not both, either way. The desktop settles the action by matching
/// what the source offers against what the receiver will take, and where both
/// are on offer the receiver decides -- and the receiver can decide either way,
/// so offering both is offering no promise at all. A move that also offered a
/// copy would be settled as a copy by a file manager; a copy that also offered
/// a move would have the original deleted by a receiver that prefers to take
/// the file, which this editor's own windows do.
fn offered_by(means: gpui::DragMeans) -> DndAction {
    match means {
        gpui::DragMeans::Taking => DndAction::Move,
        gpui::DragMeans::Copying => DndAction::Copy,
    }
}

fn files_the_drag_gave_up(action: DndAction, paths: Vec<PathBuf>) -> Vec<PathBuf> {
    match action.contains(DndAction::Move) {
        true => paths,
        false => Vec::new(),
    }
}

/// Removes what a move left behind. A file that has already gone is no error:
/// whoever took it may have moved it themselves.
fn let_go_of(paths: &[PathBuf]) {
    for path in paths {
        if !path.exists() {
            continue;
        }
        std::fs::remove_file(path)
            .or_else(|_| std::fs::remove_dir_all(path))
            .log_err();
    }
}

/// The picture for a drag of `count` files, ready to be handed to the
/// compositor. Nothing comes back when the memory for it cannot be had, and the
/// drag then goes ahead without a picture rather than not at all.
fn drag_icon_for(
    globals: &Globals,
    count: usize,
    picture: Option<&std::sync::Arc<gpui::RenderImage>>,
) -> Option<DragIcon> {
    use std::os::fd::AsFd;

    // A picture the window drew for itself says what is being carried -- the
    // file's own icon. Without one a page is drawn here, which at least says
    // that something is.
    //
    // Both are already the bytes `wl_shm` wants: premultiplied, blue first.
    let (pixels, width, height) = match picture.and_then(|picture| {
        let size = picture.size(0);
        let bytes = picture.as_bytes(0)?;
        Some((bytes.to_vec(), size.width.0, size.height.0))
    }) {
        Some(drawn) => drawn,
        None => (drag_icon_pixels(count), DRAG_ICON_SIZE, DRAG_ICON_SIZE),
    };
    let file = a_file_holding(&pixels)?;
    let pool = globals
        .shm
        .create_pool(file.as_fd(), pixels.len() as i32, &globals.qh, ());
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        width * 4,
        wl_shm::Format::Argb8888,
        &globals.qh,
        (),
    );
    let surface = globals.compositor.create_surface(&globals.qh, ());
    if surface.version() >= 3 {
        surface.set_buffer_scale(DRAG_ICON_SCALE);
    }
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, width, height);
    surface.commit();
    Some(DragIcon {
        surface,
        buffer,
        pool,
    })
}

/// The surface the compositor draws under the pointer during a drag this window
/// started, and the memory it is drawn in.
pub struct DragIcon {
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
}

impl DragIcon {
    fn let_go(self) {
        self.buffer.destroy();
        self.pool.destroy();
        self.surface.destroy();
    }
}

pub struct DragState {
    data_offer: Option<wl_data_offer::WlDataOffer>,
    window: Option<WaylandWindowStatePtr>,
    position: Point<Pixels>,
    /// Whether what this window will do with the drag amounts to taking the
    /// files. Answered once per step of the pointer, after whatever is under it
    /// has been told where the pointer is.
    takes_the_drag: bool,
    /// What the desktop last settled on for the drag now over this window. It is
    /// what the drop happens as, and with it whether whoever started the drag
    /// lets go of the original.
    action: DndAction,
}

pub struct ClickState {
    last_mouse_button: Option<MouseButton>,
    last_click: Instant,
    last_location: Point<Pixels>,
    current_count: usize,
}

pub(crate) struct KeyRepeat {
    characters_per_second: u32,
    delay: Duration,
    current_id: u64,
    current_keycode: Option<xkb::Keycode>,
}

pub(crate) enum PendingActivation {
    /// URI to open in the web browser.
    Uri(String),
    /// Path to open in the file explorer.
    Path(PathBuf),
    /// A window from ourselves to raise.
    Window(ObjectId),
}

impl WaylandClientState {
    fn consume_startup_activation_token(&mut self, surface: &wl_surface::WlSurface) {
        let Some(startup_activation_token) = self.startup_activation_token.take() else {
            return;
        };
        let Some(activation) = self.globals.activation.as_ref() else {
            return;
        };
        activation.activate(startup_activation_token, surface);
    }
}

/// This struct is required to conform to Rust's orphan rules, so we can dispatch on the state but hand the
/// window to GPUI.
#[derive(Clone)]
pub struct WaylandClientStatePtr(Weak<RefCell<WaylandClientState>>);

impl WaylandClientStatePtr {
    pub fn get_client(&self) -> Rc<RefCell<WaylandClientState>> {
        self.0
            .upgrade()
            .expect("The pointer should always be valid when dispatching in wayland")
    }

    pub fn get_serial(&self, kind: SerialKind) -> u32 {
        self.0.upgrade().unwrap().borrow().serial_tracker.get(kind)
    }

    /// Hands a drag carrying `paths` to the compositor, starting from `surface`.
    ///
    /// From here the compositor owns the drag: it decides what it lands on, and
    /// this client only hears back when something asks for the files or when the
    /// drag ends. The serial is the one of the press that is still held -- a
    /// compositor declines a drag started from any other, which is what keeps a
    /// drag from being started by a window nobody is touching.
    pub fn start_file_drag(
        &self,
        surface: &wl_surface::WlSurface,
        paths: &[PathBuf],
        picture: Option<&std::sync::Arc<gpui::RenderImage>>,
        means: gpui::DragMeans,
    ) -> bool {
        let Some(client) = self.0.upgrade() else {
            return false;
        };
        let mut state = client.borrow_mut();
        let (Some(data_device_manager), Some(data_device)) = (
            state.globals.data_device_manager.clone(),
            state.data_device.clone(),
        ) else {
            return false;
        };
        let serial = state.serial_tracker.get(SerialKind::MousePress);
        state.dragged_files = paths.to_vec();
        state.dragged_action = DndAction::empty();
        let data_source = data_device_manager.create_data_source(&state.globals.qh, DraggedOut);
        for format in formats_a_file_drag_offers() {
            data_source.offer(format.to_string());
        }
        // Saying which actions are on offer is only part of the protocol from
        // version three; an older compositor works out the action itself, and
        // asking it here would be a protocol error rather than a refusal.
        //
        // What is offered is settled here and cannot be changed afterwards --
        // the request may be made once only, and only before the drag starts.
        // So a drag that means to move offers move alone: offering copy as well
        // leaves the choice to the receiver, and a file manager receiving a drag
        // out of another application settles on copy every time. The cost is
        // that a receiver which can only copy refuses the drag outright, which
        // is why the reader can ask for a copy instead.
        if data_source.version() >= 3 {
            data_source.set_actions(offered_by(means));
        }

        let icon = drag_icon_for(&state.globals, paths.len(), picture);
        data_device.start_drag(
            Some(&data_source),
            surface,
            icon.as_ref().map(|icon| &icon.surface),
            serial,
        );
        if let Some(icon) = icon
            && let Some(gone) = state.drag_icon.replace(icon)
        {
            gone.let_go();
        }
        true
    }

    /// Says that what this window is about to do with the drag over it amounts
    /// to taking the files, so whoever started the drag lets go of the originals
    /// once it is done.
    ///
    /// Said again for every step of the drag: what is under the pointer changes
    /// as it moves, and only some of what it passes over takes the file.
    pub fn take_external_drag_as_move(&self) {
        let Some(client) = self.0.upgrade() else {
            return;
        };
        client.borrow_mut().drag.takes_the_drag = true;
    }

    /// Whether the drag being dropped on this window was settled as a move.
    pub fn external_drop_is_a_move(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|client| client.borrow().drag.action.contains(DndAction::Move))
    }

    pub fn set_pending_activation(&self, window: ObjectId) {
        self.0.upgrade().unwrap().borrow_mut().pending_activation =
            Some(PendingActivation::Window(window));
    }

    pub fn enable_ime(&self) {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        state.ime_enabled = Some(true);
        state.last_ime_cursor_rectangle = None;
        let Some(text_input) = state.text_input.take() else {
            return;
        };

        text_input.enable();
        text_input.set_content_type(ContentHint::None, ContentPurpose::Normal);
        let mut cursor_rectangle = None;
        if let Some(window) = state.keyboard_focused_window.clone() {
            drop(state);
            if let Some(area) = window.get_ime_area() {
                let area = ImeCursorRectangle::from(area);
                set_ime_cursor_rectangle(&text_input, area);
                cursor_rectangle = Some(area);
            }
            state = client.borrow_mut();
        }
        text_input.commit();
        state.last_ime_cursor_rectangle = cursor_rectangle;
        state.text_input = Some(text_input);
    }

    pub fn disable_ime(&self) {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        state.ime_enabled = Some(false);
        state.composing = false;
        if let Some(text_input) = &state.text_input {
            text_input.disable();
            text_input.commit();
        }
    }

    pub fn ime_enabled(&self) -> Option<bool> {
        let client = self.get_client();
        client.borrow().ime_enabled
    }

    pub fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        if state.pre_edit_text.is_some() {
            return;
        }
        let Some(text_input) = state.text_input.clone() else {
            return;
        };
        update_ime_cursor_rectangle(&text_input, &mut state.last_ime_cursor_rectangle, bounds);
    }

    pub fn handle_keyboard_layout_change(&self) {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        let changed = if let Some(keymap_state) = &state.keymap_state {
            let layout_idx = keymap_state.serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE);
            let keymap = keymap_state.get_keymap();
            let layout_name = keymap.layout_get_name(layout_idx);
            let changed = layout_name != state.keyboard_layout.name();
            if changed {
                state.keyboard_layout = LinuxKeyboardLayout::new(layout_name.to_string().into());
            }
            changed
        } else {
            let changed = &UNKNOWN_KEYBOARD_LAYOUT_NAME != state.keyboard_layout.name();
            if changed {
                state.keyboard_layout = LinuxKeyboardLayout::new(UNKNOWN_KEYBOARD_LAYOUT_NAME);
            }
            changed
        };

        if changed && let Some(mut callback) = state.common.callbacks.keyboard_layout_change.take()
        {
            drop(state);
            callback();
            state = client.borrow_mut();
            state.common.callbacks.keyboard_layout_change = Some(callback);
        }
    }

    pub fn drop_window(&self, surface_id: &ObjectId) {
        let client = self.get_client();
        let mut state = client.borrow_mut();
        let closed_window = state.windows.remove(surface_id).unwrap();
        if let Some(window) = state.mouse_focused_window.take()
            && !window.ptr_eq(&closed_window)
        {
            state.mouse_focused_window = Some(window);
        }
        if let Some(window) = state.keyboard_focused_window.take()
            && !window.ptr_eq(&closed_window)
        {
            state.keyboard_focused_window = Some(window);
        }
        if let Some(window) = state.cursor_hidden_window.take()
            && !window.ptr_eq(&closed_window)
        {
            state.cursor_hidden_window = Some(window);
        }
    }
}

impl WaylandClientState {
    fn hide_cursor_until_mouse_moves(&mut self) {
        if self.cursor_hidden_window.is_some() {
            return;
        }
        let Some(focused_window) = self.mouse_focused_window.clone() else {
            // No surface to apply the hidden cursor to.
            return;
        };
        let Some(wl_pointer) = self.wl_pointer.clone() else {
            // Seat lost its pointer capability; nothing to hide.
            return;
        };
        let serial = self.serial_tracker.get(SerialKind::MouseEnter);
        wl_pointer.set_cursor(serial, None, 0, 0);
        self.cursor_hidden_window = Some(focused_window);
    }

    fn restore_cursor_after_hide(&mut self) {
        if self.cursor_hidden_window.take().is_none() {
            return;
        }
        let Some(style) = self.cursor_style else {
            return;
        };
        let serial = self.serial_tracker.get(SerialKind::MouseEnter);
        if let Some(cursor_shape_device) = &self.cursor_shape_device {
            cursor_shape_device.set_shape(serial, to_shape(style));
            return;
        }
        let Some(focused_window) = self.mouse_focused_window.clone() else {
            log::warn!(
                "wayland: no focused surface to restore cursor style {:?} after hide; cursor may stay invisible",
                style
            );
            return;
        };
        let Some(wl_pointer) = self.wl_pointer.clone() else {
            log::warn!(
                "wayland: no wl_pointer to restore cursor style {:?} after hide; cursor may stay invisible",
                style
            );
            return;
        };
        let scale = focused_window.primary_output_scale();
        self.cursor.set_icon(
            &wl_pointer,
            serial,
            cursor_style_to_icon_names(style),
            scale,
        );
    }
}

#[derive(Clone)]
pub struct WaylandClient(Rc<RefCell<WaylandClientState>>);

impl Drop for WaylandClient {
    fn drop(&mut self) {
        let mut state = self.0.borrow_mut();
        state.windows.clear();

        if let Some(wl_pointer) = &state.wl_pointer {
            wl_pointer.release();
        }
        if let Some(cursor_shape_device) = &state.cursor_shape_device {
            cursor_shape_device.destroy();
        }
        if let Some(data_device) = &state.data_device {
            data_device.release();
        }
        if let Some(text_input) = &state.text_input {
            text_input.destroy();
        }
    }
}

const WL_DATA_DEVICE_MANAGER_VERSION: u32 = 3;

fn wl_seat_version(version: u32) -> u32 {
    // We rely on the wl_pointer.frame event
    const WL_SEAT_MIN_VERSION: u32 = 5;
    const WL_SEAT_MAX_VERSION: u32 = 9;

    if version < WL_SEAT_MIN_VERSION {
        panic!(
            "wl_seat below required version: {} < {}",
            version, WL_SEAT_MIN_VERSION
        );
    }

    version.clamp(WL_SEAT_MIN_VERSION, WL_SEAT_MAX_VERSION)
}

fn wl_output_version(version: u32) -> u32 {
    const WL_OUTPUT_MIN_VERSION: u32 = 2;
    const WL_OUTPUT_MAX_VERSION: u32 = 4;

    if version < WL_OUTPUT_MIN_VERSION {
        panic!(
            "wl_output below required version: {} < {}",
            version, WL_OUTPUT_MIN_VERSION
        );
    }

    version.clamp(WL_OUTPUT_MIN_VERSION, WL_OUTPUT_MAX_VERSION)
}

impl WaylandClient {
    pub(crate) fn new() -> Self {
        let startup_activation_token = take_startup_activation_token_from_environment();
        let conn = Connection::connect_to_env().unwrap();

        let (globals, event_queue) = registry_queue_init::<WaylandClientStatePtr>(&conn).unwrap();
        let qh = event_queue.handle();

        let mut seat: Option<wl_seat::WlSeat> = None;
        #[allow(clippy::mutable_key_type)]
        let mut in_progress_outputs = HashMap::default();
        #[allow(clippy::mutable_key_type)]
        let mut wl_outputs: HashMap<ObjectId, wl_output::WlOutput> = HashMap::default();
        globals.contents().with_list(|list| {
            for global in list {
                match &global.interface[..] {
                    "wl_seat" => {
                        seat = Some(globals.registry().bind::<wl_seat::WlSeat, _, _>(
                            global.name,
                            wl_seat_version(global.version),
                            &qh,
                            (),
                        ));
                    }
                    "wl_output" => {
                        let output = globals.registry().bind::<wl_output::WlOutput, _, _>(
                            global.name,
                            wl_output_version(global.version),
                            &qh,
                            (),
                        );
                        in_progress_outputs.insert(output.id(), InProgressOutput::default());
                        wl_outputs.insert(output.id(), output);
                    }
                    _ => {}
                }
            }
        });

        let event_loop = EventLoop::<WaylandClientStatePtr>::try_new().unwrap();

        let (common, main_receiver, wake_receiver) = LinuxCommon::new(event_loop.get_signal());

        let handle = event_loop.handle();
        handle
            .insert_source(main_receiver, {
                let handle = handle.clone();
                move |event, _, _: &mut WaylandClientStatePtr| {
                    if let calloop::channel::Event::Msg(runnable) = event {
                        handle.insert_idle(|_| {
                            let location = runnable.metadata().location;
                            let spawned = runnable.metadata().spawned;
                            profiler::update_running_task(spawned, location);
                            runnable.run();
                            profiler::save_task_timing();
                        });
                    }
                }
            })
            .unwrap();

        handle
            .insert_source(
                wake_receiver,
                |event, _, client: &mut WaylandClientStatePtr| {
                    if let calloop::channel::Event::Msg(()) = event {
                        client.get_client().borrow_mut().common.handle_system_wake();
                    }
                },
            )
            .unwrap();

        let compositor_gpu = detect_compositor_gpu();
        let gpu_context = Rc::new(RefCell::new(None));

        let seat = seat.unwrap();
        let globals = Globals::new(
            globals,
            common.foreground_executor.clone(),
            qh.clone(),
            seat.clone(),
        );

        let data_device = globals
            .data_device_manager
            .as_ref()
            .map(|data_device_manager| data_device_manager.get_data_device(&seat, &qh, ()));

        let primary_selection = globals
            .primary_selection_manager
            .as_ref()
            .map(|primary_selection_manager| primary_selection_manager.get_device(&seat, &qh, ()));

        let cursor = Cursor::new(&conn, &globals, 24);

        handle
            .insert_source(XDPEventSource::new(&common.background_executor), {
                move |event, _, client| match event {
                    XDPEvent::WindowAppearance(appearance) => {
                        if let Some(client) = client.0.upgrade() {
                            let mut client = client.borrow_mut();

                            client.common.appearance = appearance;

                            for window in client.windows.values_mut() {
                                window.set_appearance(appearance);
                            }
                        }
                    }
                    XDPEvent::ButtonLayout(layout_str) => {
                        if let Some(client) = client.0.upgrade() {
                            let layout = WindowButtonLayout::parse(&layout_str)
                                .log_err()
                                .unwrap_or_else(WindowButtonLayout::linux_default);
                            let mut client = client.borrow_mut();
                            client.common.button_layout = layout;

                            for window in client.windows.values_mut() {
                                window.set_button_layout();
                            }
                        }
                    }
                    XDPEvent::CursorTheme(theme) => {
                        if let Some(client) = client.0.upgrade() {
                            let mut client = client.borrow_mut();
                            client.cursor.set_theme(theme);
                        }
                    }
                    XDPEvent::CursorSize(size) => {
                        if let Some(client) = client.0.upgrade() {
                            let mut client = client.borrow_mut();
                            client.cursor.set_size(size);
                        }
                    }
                }
            })
            .unwrap();

        let state = Rc::new(RefCell::new(WaylandClientState {
            serial_tracker: SerialTracker::new(),
            globals,
            gpu_context,
            compositor_gpu,
            wl_seat: seat,
            wl_pointer: None,
            wl_keyboard: None,
            pinch_gesture: None,
            pinch_scale: 1.0,
            cursor_shape_device: None,
            data_device,
            dragged_files: Vec::new(),
            dragged_action: DndAction::empty(),
            drag_icon: None,
            primary_selection,
            text_input: None,
            pre_edit_text: None,
            ime_pre_edit: None,
            composing: false,
            last_ime_cursor_rectangle: None,
            outputs: HashMap::default(),
            in_progress_outputs,
            wl_outputs,
            windows: HashMap::default(),
            common,
            keyboard_layout: LinuxKeyboardLayout::new(UNKNOWN_KEYBOARD_LAYOUT_NAME),
            keymap_state: None,
            compose_state: None,
            drag: DragState {
                data_offer: None,
                window: None,
                takes_the_drag: false,
                action: DndAction::empty(),
                position: Point::default(),
            },
            click: ClickState {
                last_click: Instant::now(),
                last_mouse_button: None,
                last_location: Point::default(),
                current_count: 0,
            },
            repeat: KeyRepeat {
                characters_per_second: 16,
                delay: Duration::from_millis(500),
                current_id: 0,
                current_keycode: None,
            },
            modifiers: Modifiers {
                shift: false,
                control: false,
                alt: false,
                function: false,
                platform: false,
            },
            capslock: Capslock { on: false },
            scroll_event_received: false,
            axis_source: AxisSource::Wheel,
            mouse_location: None,
            continuous_scroll_delta: None,
            discrete_scroll_delta: None,
            vertical_modifier: -1.0,
            horizontal_modifier: -1.0,
            button_pressed: None,
            mouse_focused_window: None,
            keyboard_focused_window: None,
            loop_handle: handle.clone(),
            enter_token: None,
            cursor_style: None,
            cursor_hidden_window: None,
            clipboard: Clipboard::new(conn.clone(), handle.clone()),
            data_offers: Vec::new(),
            primary_data_offer: None,
            cursor,
            pending_activation: None,
            startup_activation_token,
            event_loop: Some(event_loop),
            ime_enabled: None,
        }));

        WaylandSource::new(conn, event_queue)
            .insert(handle)
            .unwrap();

        Self(state)
    }
}

impl LinuxClient for WaylandClient {
    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(self.0.borrow().keyboard_layout.clone())
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        self.0
            .borrow()
            .outputs
            .iter()
            .map(|(id, output)| {
                Rc::new(WaylandDisplay {
                    id: id.clone(),
                    name: output.name.clone(),
                    bounds: output.bounds.to_pixels(output.scale as f32),
                }) as Rc<dyn PlatformDisplay>
            })
            .collect()
    }

    fn display(&self, id: DisplayId) -> Option<Rc<dyn PlatformDisplay>> {
        self.0
            .borrow()
            .outputs
            .iter()
            .find_map(|(object_id, output)| {
                (object_id.protocol_id() as u64 == u64::from(id)).then(|| {
                    Rc::new(WaylandDisplay {
                        id: object_id.clone(),
                        name: output.name.clone(),
                        bounds: output.bounds.to_pixels(output.scale as f32),
                    }) as Rc<dyn PlatformDisplay>
                })
            })
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        None
    }

    #[cfg(feature = "screen-capture")]
    fn screen_capture_sources(
        &self,
    ) -> futures::channel::oneshot::Receiver<anyhow::Result<Vec<Rc<dyn gpui::ScreenCaptureSource>>>>
    {
        // TODO: Get screen capture working on wayland. Be sure to try window resizing as that may
        // be tricky.
        //
        // start_scap_default_target_source()
        let (sources_tx, sources_rx) = futures::channel::oneshot::channel();
        sources_tx
            .send(Err(anyhow::anyhow!(
                "Wayland screen capture not yet implemented."
            )))
            .ok();
        sources_rx
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let mut state = self.0.borrow_mut();

        // Popups name their parent explicitly. Other kinds are parented to the focused window.
        let (parent, popup_grab) = match &params.kind {
            WindowKind::AnchoredPopup(options) => {
                let parent = state
                    .windows
                    .values()
                    .find(|window| window.handle() == options.parent)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("popup parent window not found"))?;
                // A popup grab must reference a press event or the compositor declines it and
                // immediately dismisses the popup, so use the most recent press serial, or no
                // grab before any press.
                let popup_grab = options.grab.then(|| {
                    let serial = state
                        .serial_tracker
                        .get(SerialKind::MousePress)
                        .max(state.serial_tracker.get(SerialKind::KeyPress));
                    (serial != 0).then(|| (serial, state.wl_seat.clone()))
                });
                (Some(parent), popup_grab.flatten())
            }
            _ => (state.keyboard_focused_window.clone(), None),
        };

        let target_output = params.display_id.and_then(|display_id| {
            let target_protocol_id: u64 = display_id.into();
            state
                .wl_outputs
                .iter()
                .find(|(id, _)| id.protocol_id() as u64 == target_protocol_id)
                .map(|(_, output)| output.clone())
        });

        let appearance = state.common.appearance;
        let compositor_gpu = state.compositor_gpu.take();

        let (window, surface_id) = WaylandWindow::new(
            handle,
            state.globals.clone(),
            state.gpu_context.clone(),
            compositor_gpu,
            WaylandClientStatePtr(Rc::downgrade(&self.0)),
            params,
            appearance,
            parent,
            popup_grab,
            target_output,
        )?;

        if window.0.toplevel().is_some() {
            state.consume_startup_activation_token(&window.0.surface());
        }
        state.windows.insert(surface_id, window.0.clone());

        Ok(Box::new(window))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        let mut state = self.0.borrow_mut();

        let need_update = state.cursor_style != Some(style)
            && (state.mouse_focused_window.is_none()
                || state
                    .mouse_focused_window
                    .as_ref()
                    .is_some_and(|w| !w.is_blocked()));

        if !need_update {
            return;
        }

        state.cursor_style = Some(style);

        // Don't clobber the invisible cursor; restore reads back from `cursor_style`.
        if state.cursor_hidden_window.is_some() {
            return;
        }

        let serial = state.serial_tracker.get(SerialKind::MouseEnter);
        if let Some(cursor_shape_device) = &state.cursor_shape_device {
            cursor_shape_device.set_shape(serial, to_shape(style));
        } else if let Some(focused_window) = &state.mouse_focused_window {
            // cursor-shape-v1 isn't supported, set the cursor using a surface.
            let wl_pointer = state
                .wl_pointer
                .clone()
                .expect("window is focused by pointer");
            let scale = focused_window.primary_output_scale();
            state.cursor.set_icon(
                &wl_pointer,
                serial,
                cursor_style_to_icon_names(style),
                scale,
            );
        }
    }

    fn hide_cursor_until_mouse_moves(&self) {
        self.0.borrow_mut().hide_cursor_until_mouse_moves();
    }

    fn is_cursor_visible(&self) -> bool {
        self.0.borrow().cursor_hidden_window.is_none()
    }

    fn open_uri(&self, uri: &str) {
        let mut state = self.0.borrow_mut();
        if let (Some(activation), Some(window)) = (
            state.globals.activation.clone(),
            state.mouse_focused_window.clone(),
        ) {
            state.pending_activation = Some(PendingActivation::Uri(uri.to_string()));
            let token = activation.get_activation_token(&state.globals.qh, ());
            let serial = state.serial_tracker.get(SerialKind::MousePress);
            token.set_serial(serial, &state.wl_seat);
            token.set_surface(&window.surface());
            token.commit();
        } else {
            let executor = state.common.background_executor.clone();
            open_uri_internal(executor, uri, None);
        }
    }

    fn reveal_path(&self, path: PathBuf) {
        let mut state = self.0.borrow_mut();
        if let (Some(activation), Some(window)) = (
            state.globals.activation.clone(),
            state.mouse_focused_window.clone(),
        ) {
            state.pending_activation = Some(PendingActivation::Path(path));
            let token = activation.get_activation_token(&state.globals.qh, ());
            let serial = state.serial_tracker.get(SerialKind::MousePress);
            token.set_serial(serial, &state.wl_seat);
            token.set_surface(&window.surface());
            token.commit();
        } else {
            let executor = state.common.background_executor.clone();
            reveal_path_internal(executor, path, None);
        }
    }

    fn with_common<R>(&self, f: impl FnOnce(&mut LinuxCommon) -> R) -> R {
        f(&mut self.0.borrow_mut().common)
    }

    fn run(&self) {
        let mut event_loop = self
            .0
            .borrow_mut()
            .event_loop
            .take()
            .expect("App is already running");

        event_loop
            .run(
                None,
                &mut WaylandClientStatePtr(Rc::downgrade(&self.0)),
                |_| {},
            )
            .log_err();
    }

    fn write_to_primary(&self, item: gpui::ClipboardItem) {
        let mut state = self.0.borrow_mut();
        let (Some(primary_selection_manager), Some(primary_selection)) = (
            state.globals.primary_selection_manager.clone(),
            state.primary_selection.clone(),
        ) else {
            return;
        };
        if state.mouse_focused_window.is_some() || state.keyboard_focused_window.is_some() {
            let offer_files = external_paths(&item).is_some();
            state.clipboard.set_primary(item);
            let serial = state.serial_tracker.get_latest();
            let data_source = primary_selection_manager.create_source(&state.globals.qh, ());
            for mime_type in TEXT_MIME_TYPES {
                data_source.offer(mime_type.to_string());
            }
            if offer_files {
                data_source.offer(FILE_LIST_MIME_TYPE.to_string());
                data_source.offer(GNOME_COPIED_FILES_MIME_TYPE.to_string());
            }
            data_source.offer(state.clipboard.self_mime());
            primary_selection.set_selection(Some(&data_source), serial);
        }
    }

    fn write_to_clipboard(&self, item: gpui::ClipboardItem) {
        let mut state = self.0.borrow_mut();
        let (Some(data_device_manager), Some(data_device)) = (
            state.globals.data_device_manager.clone(),
            state.data_device.clone(),
        ) else {
            return;
        };
        if state.mouse_focused_window.is_some() || state.keyboard_focused_window.is_some() {
            let offer_files = external_paths(&item).is_some();
            state.clipboard.set(item);
            let serial = state.serial_tracker.get_latest();
            let data_source = data_device_manager.create_data_source(&state.globals.qh, ());
            for mime_type in TEXT_MIME_TYPES {
                data_source.offer(mime_type.to_string());
            }
            if offer_files {
                data_source.offer(FILE_LIST_MIME_TYPE.to_string());
                data_source.offer(GNOME_COPIED_FILES_MIME_TYPE.to_string());
            }
            data_source.offer(state.clipboard.self_mime());
            data_device.set_selection(Some(&data_source), serial);
        }
    }

    fn read_from_primary(&self) -> Option<gpui::ClipboardItem> {
        self.0.borrow_mut().clipboard.read_primary()
    }

    fn read_from_clipboard(&self) -> Option<gpui::ClipboardItem> {
        self.0.borrow_mut().clipboard.read()
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.0
            .borrow_mut()
            .keyboard_focused_window
            .as_ref()
            .map(|window| window.handle())
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        None
    }

    fn compositor_name(&self) -> &'static str {
        "Wayland"
    }

    fn window_identifier(&self) -> impl Future<Output = Option<WindowIdentifier>> + Send + 'static {
        async fn inner(surface: Option<wl_surface::WlSurface>) -> Option<WindowIdentifier> {
            if let Some(surface) = surface {
                ashpd::WindowIdentifier::from_wayland(&surface).await
            } else {
                None
            }
        }

        let client_state = self.0.borrow();
        let active_window = client_state.keyboard_focused_window.as_ref();
        inner(active_window.map(|aw| aw.surface()))
    }
}

struct DmabufProbeState {
    device: Option<u64>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for DmabufProbeState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for DmabufProbeState {
    fn event(
        _: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        _: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for DmabufProbeState {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } = event {
            if let Ok(bytes) = <[u8; 8]>::try_from(device.as_slice()) {
                state.device = Some(u64::from_ne_bytes(bytes));
            }
        }
    }
}

fn detect_compositor_gpu() -> Option<CompositorGpuHint> {
    let connection = Connection::connect_to_env().ok()?;
    let (globals, mut event_queue) = registry_queue_init::<DmabufProbeState>(&connection).ok()?;
    let queue_handle = event_queue.handle();

    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 =
        globals.bind(&queue_handle, 4..=4, ()).ok()?;
    let feedback = dmabuf.get_default_feedback(&queue_handle, ());

    let mut state = DmabufProbeState { device: None };

    event_queue.roundtrip(&mut state).ok()?;

    feedback.destroy();
    dmabuf.destroy();

    crate::linux::compositor_gpu_hint_from_dev_t(state.device?)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match &interface[..] {
                "wl_seat" => {
                    if let Some(wl_pointer) = state.wl_pointer.take() {
                        wl_pointer.release();
                    }
                    if let Some(wl_keyboard) = state.wl_keyboard.take() {
                        wl_keyboard.release();
                    }
                    state.wl_seat.release();
                    state.wl_seat = registry.bind::<wl_seat::WlSeat, _, _>(
                        name,
                        wl_seat_version(version),
                        qh,
                        (),
                    );
                }
                "wl_output" => {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        wl_output_version(version),
                        qh,
                        (),
                    );

                    state
                        .in_progress_outputs
                        .insert(output.id(), InProgressOutput::default());
                    state.wl_outputs.insert(output.id(), output);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name: _ } => {
                // TODO: handle global removal
            }
            _ => {}
        }
    }
}

delegate_noop!(WaylandClientStatePtr: ignore xdg_activation_v1::XdgActivationV1);
delegate_noop!(WaylandClientStatePtr: ignore xdg_system_bell_v1::XdgSystemBellV1);
delegate_noop!(WaylandClientStatePtr: ignore wl_compositor::WlCompositor);
delegate_noop!(WaylandClientStatePtr: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
delegate_noop!(WaylandClientStatePtr: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(WaylandClientStatePtr: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(WaylandClientStatePtr: ignore zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1);
delegate_noop!(WaylandClientStatePtr: ignore wl_shm::WlShm);
delegate_noop!(WaylandClientStatePtr: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WaylandClientStatePtr: ignore wl_buffer::WlBuffer);
delegate_noop!(WaylandClientStatePtr: ignore wl_region::WlRegion);
delegate_noop!(WaylandClientStatePtr: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(WaylandClientStatePtr: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(WaylandClientStatePtr: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(WaylandClientStatePtr: ignore xdg_positioner::XdgPositioner);
delegate_noop!(WaylandClientStatePtr: ignore org_kde_kwin_blur_manager::OrgKdeKwinBlurManager);
delegate_noop!(WaylandClientStatePtr: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
delegate_noop!(WaylandClientStatePtr: ignore org_kde_kwin_blur::OrgKdeKwinBlur);
delegate_noop!(WaylandClientStatePtr: ignore wp_viewporter::WpViewporter);
delegate_noop!(WaylandClientStatePtr: ignore wp_viewport::WpViewport);

impl Dispatch<WlCallback, ObjectId> for WaylandClientStatePtr {
    fn event(
        state: &mut WaylandClientStatePtr,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        surface_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = state.get_client();
        let mut state = client.borrow_mut();
        let Some(window) = get_window(&mut state, surface_id) else {
            return;
        };
        drop(state);

        if let wl_callback::Event::Done { .. } = event {
            window.frame();
        }
    }
}

pub(crate) fn get_window(
    state: &mut RefMut<WaylandClientState>,
    surface_id: &ObjectId,
) -> Option<WaylandWindowStatePtr> {
    state.windows.get(surface_id).cloned()
}

impl Dispatch<wl_surface::WlSurface, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        surface: &wl_surface::WlSurface,
        event: <wl_surface::WlSurface as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        let Some(window) = get_window(&mut state, &surface.id()) else {
            return;
        };
        #[allow(clippy::mutable_key_type)]
        let outputs = state.outputs.clone();
        drop(state);

        window.handle_surface_event(event, outputs);
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        output: &wl_output::WlOutput,
        event: <wl_output::WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        let Some(in_progress_output) = state.in_progress_outputs.get_mut(&output.id()) else {
            return;
        };

        match event {
            wl_output::Event::Name { name } => {
                in_progress_output.name = Some(name);
            }
            wl_output::Event::Scale { factor } => {
                in_progress_output.scale = Some(factor);
            }
            wl_output::Event::Geometry { x, y, subpixel, .. } => {
                in_progress_output.position = Some(point(DevicePixels(x), DevicePixels(y)));
                if let WEnum::Value(subpixel) = subpixel {
                    in_progress_output.subpixel = Some(subpixel);
                }
            }
            wl_output::Event::Mode { width, height, .. } => {
                in_progress_output.size = Some(size(DevicePixels(width), DevicePixels(height)))
            }
            wl_output::Event::Done => {
                if let Some(complete) = in_progress_output.complete() {
                    state.outputs.insert(output.id(), complete);
                }
                state.in_progress_outputs.remove(&output.id());
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ObjectId> for WaylandClientStatePtr {
    fn event(
        state: &mut Self,
        _: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        surface_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = state.get_client();
        let mut state = client.borrow_mut();
        let Some(window) = get_window(&mut state, surface_id) else {
            return;
        };
        drop(state);
        window.handle_xdg_surface_event(event);
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ObjectId> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: <xdg_toplevel::XdgToplevel as Proxy>::Event,
        surface_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        let Some(window) = get_window(&mut state, surface_id) else {
            return;
        };

        drop(state);
        let should_close = window.handle_toplevel_event(event);

        if should_close {
            // The close logic will be handled in drop_window()
            window.close();
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ObjectId> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: <zwlr_layer_surface_v1::ZwlrLayerSurfaceV1 as Proxy>::Event,
        surface_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        let Some(window) = get_window(&mut state, surface_id) else {
            return;
        };

        drop(state);
        let should_close = window.handle_layersurface_event(event);

        if should_close {
            // Close logic will be handled in drop_window()
            window.close();
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ObjectId> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: <xdg_popup::XdgPopup as Proxy>::Event,
        surface_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        let Some(window) = get_window(&mut state, surface_id) else {
            return;
        };

        drop(state);
        let should_close = window.handle_popup_event(event);

        if should_close {
            // The close logic will be handled in drop_window()
            window.close();
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandClientStatePtr {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: <xdg_wm_base::XdgWmBase as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        token: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: <xdg_activation_token_v1::XdgActivationTokenV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        if let xdg_activation_token_v1::Event::Done { token } = event {
            let executor = state.common.background_executor.clone();
            match state.pending_activation.take() {
                Some(PendingActivation::Uri(uri)) => open_uri_internal(executor, &uri, Some(token)),
                Some(PendingActivation::Path(path)) => {
                    reveal_path_internal(executor, path, Some(token))
                }
                Some(PendingActivation::Window(window)) => {
                    let Some(window) = get_window(&mut state, &window) else {
                        return;
                    };
                    let activation = state.globals.activation.as_ref().unwrap();
                    activation.activate(token, &window.surface());
                }
                None => log::error!("activation token received with no pending activation"),
            }
        }

        token.destroy();
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandClientStatePtr {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            let client = state.get_client();
            let mut state = client.borrow_mut();
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                let keyboard = seat.get_keyboard(qh, ());

                if let Some(text_input) = state.text_input.take() {
                    text_input.destroy();
                    state.ime_pre_edit = None;
                    state.composing = false;
                }

                state.text_input = state
                    .globals
                    .text_input_manager
                    .as_ref()
                    .map(|text_input_manager| text_input_manager.get_text_input(seat, qh, ()));

                if let Some(wl_keyboard) = &state.wl_keyboard {
                    wl_keyboard.release();
                }

                state.wl_keyboard = Some(keyboard);
            }
            if capabilities.contains(wl_seat::Capability::Pointer) {
                let pointer = seat.get_pointer(qh, ());

                if let Some(cursor_shape_device) = state.cursor_shape_device.take() {
                    cursor_shape_device.destroy();
                }

                state.cursor_shape_device = state
                    .globals
                    .cursor_shape_manager
                    .as_ref()
                    .map(|cursor_shape_manager| cursor_shape_manager.get_pointer(&pointer, qh, ()));

                state.pinch_gesture = state.globals.gesture_manager.as_ref().map(
                    |gesture_manager: &zwp_pointer_gestures_v1::ZwpPointerGesturesV1| {
                        gesture_manager.get_pinch_gesture(&pointer, qh, ())
                    },
                );

                if let Some(wl_pointer) = &state.wl_pointer {
                    wl_pointer.release();
                }

                state.wl_pointer = Some(pointer);
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        match event {
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat.characters_per_second = rate as u32;
                state.repeat.delay = Duration::from_millis(delay as u64);
            }
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(format),
                fd,
                size,
                ..
            } => {
                if format != wl_keyboard::KeymapFormat::XkbV1 {
                    log::error!("Received keymap format {:?}, expected XkbV1", format);
                    return;
                }
                let xkb_context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &xkb_context,
                        fd,
                        size as usize,
                        XKB_KEYMAP_FORMAT_TEXT_V1,
                        KEYMAP_COMPILE_NO_FLAGS,
                    )
                    .log_err()
                    .flatten()
                    .expect("Failed to create keymap")
                };
                state.keymap_state = Some(xkb::State::new(&keymap));
                state.compose_state = get_xkb_compose_state(&xkb_context);
                drop(state);

                this.handle_keyboard_layout_change();
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_focused_window = get_window(&mut state, &surface.id());
                state.enter_token = Some(());

                if let Some(window) = state.keyboard_focused_window.clone() {
                    drop(state);
                    window.set_focused(true);
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                let keyboard_focused_window = get_window(&mut state, &surface.id());
                state.keyboard_focused_window = None;
                state.enter_token.take();
                // Prevent keyboard events from repeating after opening e.g. a file chooser and closing it quickly
                state.repeat.current_id += 1;
                state.restore_cursor_after_hide();

                if let Some(window) = keyboard_focused_window {
                    if let Some(ref mut compose) = state.compose_state {
                        compose.reset();
                    }
                    state.pre_edit_text.take();
                    drop(state);
                    window.handle_ime(ImeInput::DeleteText);
                    window.set_focused(false);
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                let focused_window = state.keyboard_focused_window.clone();

                let keymap_state = state.keymap_state.as_mut().unwrap();
                let old_layout =
                    keymap_state.serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE);
                keymap_state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                state.modifiers = modifiers_from_xkb(keymap_state);
                let keymap_state = state.keymap_state.as_mut().unwrap();
                state.capslock = capslock_from_xkb(keymap_state);

                let input = PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers: state.modifiers,
                    capslock: state.capslock,
                });
                drop(state);

                if let Some(focused_window) = focused_window {
                    focused_window.handle_input(input);
                }

                if group != old_layout {
                    this.handle_keyboard_layout_change();
                }
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                state.serial_tracker.update(SerialKind::KeyPress, serial);

                let focused_window = state.keyboard_focused_window.clone();
                let Some(focused_window) = focused_window else {
                    return;
                };

                let keymap_state = state.keymap_state.as_ref().unwrap();
                let keycode = Keycode::from(key + MIN_KEYCODE);
                let keysym = keymap_state.key_get_one_sym(keycode);

                match key_state {
                    wl_keyboard::KeyState::Pressed if !keysym.is_modifier_key() => {
                        let mut keystroke =
                            keystroke_from_xkb(keymap_state, state.modifiers, keycode);
                        if let Some(mut compose) = state.compose_state.take() {
                            compose.feed(keysym);
                            match compose.status() {
                                xkb::Status::Composing => {
                                    keystroke.key_char = None;
                                    state.pre_edit_text =
                                        compose.utf8().or(keystroke_underlying_dead_key(keysym));
                                    let pre_edit =
                                        state.pre_edit_text.clone().unwrap_or(String::default());
                                    drop(state);
                                    focused_window.handle_ime(ImeInput::SetMarkedText(pre_edit));
                                    state = client.borrow_mut();
                                }

                                xkb::Status::Composed => {
                                    state.pre_edit_text.take();
                                    keystroke.key_char = compose.utf8();
                                    if let Some(keysym) = compose.keysym() {
                                        keystroke.key = xkb::keysym_get_name(keysym);
                                    }
                                }
                                xkb::Status::Cancelled => {
                                    let pre_edit = state.pre_edit_text.take();
                                    let new_pre_edit = keystroke_underlying_dead_key(keysym);
                                    state.pre_edit_text = new_pre_edit.clone();
                                    drop(state);
                                    if let Some(pre_edit) = pre_edit {
                                        focused_window.handle_ime(ImeInput::InsertText(pre_edit));
                                    }
                                    if let Some(current_key) = new_pre_edit {
                                        focused_window
                                            .handle_ime(ImeInput::SetMarkedText(current_key));
                                    }
                                    compose.feed(keysym);
                                    state = client.borrow_mut();
                                }
                                _ => {}
                            }
                            state.compose_state = Some(compose);
                        }
                        let input = PlatformInput::KeyDown(KeyDownEvent {
                            keystroke: keystroke.clone(),
                            is_held: false,
                            prefer_character_input: false,
                        });

                        state.repeat.current_id += 1;
                        state.repeat.current_keycode = Some(keycode);

                        let rate = state.repeat.characters_per_second;
                        let repeat_interval = Duration::from_secs(1) / rate.max(1);
                        let id = state.repeat.current_id;
                        state
                            .loop_handle
                            .insert_source(Timer::from_duration(state.repeat.delay), {
                                let input = PlatformInput::KeyDown(KeyDownEvent {
                                    keystroke,
                                    is_held: true,
                                    prefer_character_input: false,
                                });
                                move |event_timestamp, _metadata, this| {
                                    let client = this.get_client();
                                    let state = client.borrow();
                                    let is_repeating = id == state.repeat.current_id
                                        && state.repeat.current_keycode.is_some()
                                        && state.keyboard_focused_window.is_some();

                                    if !is_repeating || rate == 0 {
                                        return TimeoutAction::Drop;
                                    }

                                    let focused_window =
                                        state.keyboard_focused_window.as_ref().unwrap().clone();

                                    drop(state);
                                    focused_window.handle_input(input.clone());

                                    // If the new scheduled time is in the past the event will repeat as soon as possible
                                    TimeoutAction::ToInstant(event_timestamp + repeat_interval)
                                }
                            })
                            .unwrap();

                        drop(state);
                        focused_window.handle_input(input);
                    }
                    wl_keyboard::KeyState::Released if !keysym.is_modifier_key() => {
                        let input = PlatformInput::KeyUp(KeyUpEvent {
                            keystroke: keystroke_from_xkb(keymap_state, state.modifiers, keycode),
                        });

                        if state.repeat.current_keycode == Some(keycode) {
                            state.repeat.current_keycode = None;
                        }

                        drop(state);
                        focused_window.handle_input(input);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        text_input: &zwp_text_input_v3::ZwpTextInputV3,
        event: <zwp_text_input_v3::ZwpTextInputV3 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        match event {
            zwp_text_input_v3::Event::Enter { .. } => {
                drop(state);
                this.enable_ime();
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                drop(state);
                this.disable_ime();
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.composing = false;
                let Some(window) = state.keyboard_focused_window.clone() else {
                    return;
                };

                if let Some(commit_text) = text {
                    drop(state);
                    // IBus Intercepts keys like `a`, `b`, but those keys are needed for vim mode.
                    // We should only send ASCII characters to Zed, otherwise a user could remap a letter like `か` or `相`.
                    if commit_text.len() == 1 {
                        window.handle_input(PlatformInput::KeyDown(KeyDownEvent {
                            keystroke: Keystroke {
                                modifiers: Modifiers::default(),
                                key: commit_text.clone(),
                                key_char: Some(commit_text),
                            },
                            is_held: false,
                            prefer_character_input: false,
                        }));
                    } else {
                        window.handle_ime(ImeInput::InsertText(commit_text));
                    }
                }
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                state.composing = true;
                state.ime_pre_edit = text;
            }
            zwp_text_input_v3::Event::Done { serial } => {
                let last_serial = state.serial_tracker.get(SerialKind::InputMethod);
                state.serial_tracker.update(SerialKind::InputMethod, serial);
                let Some(window) = state.keyboard_focused_window.clone() else {
                    return;
                };

                if let Some(text) = state.ime_pre_edit.take() {
                    drop(state);
                    window.handle_ime(ImeInput::SetMarkedText(text));
                    if let Some(area) = window.get_ime_area() {
                        let mut state = client.borrow_mut();
                        set_ime_cursor_rectangle_after_done(
                            text_input,
                            &mut state.last_ime_cursor_rectangle,
                            area,
                            last_serial == serial,
                        );
                    }
                } else {
                    state.composing = false;
                    drop(state);
                    window.handle_ime(ImeInput::DeleteText);
                }
            }
            _ => {}
        }
    }
}

fn linux_button_to_gpui(button: u32) -> Option<MouseButton> {
    // These values are coming from <linux/input-event-codes.h>.
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    const BTN_SIDE: u32 = 0x113;
    const BTN_EXTRA: u32 = 0x114;
    const BTN_FORWARD: u32 = 0x115;
    const BTN_BACK: u32 = 0x116;

    Some(match button {
        BTN_LEFT => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_BACK | BTN_SIDE => MouseButton::Navigate(NavigationDirection::Back),
        BTN_FORWARD | BTN_EXTRA => MouseButton::Navigate(NavigationDirection::Forward),
        _ => return None,
    })
}

impl Dispatch<wl_pointer::WlPointer, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        wl_pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                let position = point(px(surface_x as f32), px(surface_y as f32));
                state.serial_tracker.update(SerialKind::MouseEnter, serial);
                state.mouse_location = Some(position);
                state.button_pressed = None;

                if let Some(window) = get_window(&mut state, &surface.id()) {
                    state.mouse_focused_window = Some(window.clone());

                    if state.enter_token.is_some() {
                        state.enter_token = None;
                    }
                    state.restore_cursor_after_hide();
                    if let Some(style) = state.cursor_style {
                        if let Some(cursor_shape_device) = &state.cursor_shape_device {
                            cursor_shape_device.set_shape(serial, to_shape(style));
                        } else {
                            let scale = window.primary_output_scale();
                            state.cursor.set_icon(
                                wl_pointer,
                                serial,
                                cursor_style_to_icon_names(style),
                                scale,
                            );
                        }
                    }
                    let modifiers = state.modifiers;
                    drop(state);
                    window.set_hovered(true);
                    // No Motion follows Enter unless the pointer keeps moving, so synthesize
                    // a MouseMove to establish hover at the entry position.
                    window.handle_input(PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers,
                    }));
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if let Some(focused_window) = state.mouse_focused_window.clone() {
                    let input = PlatformInput::MouseExited(MouseExitEvent {
                        position: state.mouse_location.unwrap(),
                        pressed_button: state.button_pressed,
                        modifiers: state.modifiers,
                    });
                    state.mouse_focused_window = None;
                    state.mouse_location = None;
                    state.button_pressed = None;
                    state.cursor_hidden_window = None;

                    drop(state);
                    focused_window.handle_input(input);
                    focused_window.set_hovered(false);
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if state.mouse_focused_window.is_none() {
                    return;
                }
                state.mouse_location = Some(point(px(surface_x as f32), px(surface_y as f32)));
                state.restore_cursor_after_hide();

                if let Some(window) = state.mouse_focused_window.clone() {
                    if window.is_blocked() {
                        let default_style = CursorStyle::Arrow;
                        if state.cursor_style != Some(default_style) {
                            let serial = state.serial_tracker.get(SerialKind::MouseEnter);
                            state.cursor_style = Some(default_style);

                            if let Some(cursor_shape_device) = &state.cursor_shape_device {
                                cursor_shape_device.set_shape(serial, to_shape(default_style));
                            } else {
                                // cursor-shape-v1 isn't supported, set the cursor using a surface.
                                let wl_pointer = state
                                    .wl_pointer
                                    .clone()
                                    .expect("window is focused by pointer");
                                let scale = window.primary_output_scale();
                                state.cursor.set_icon(
                                    &wl_pointer,
                                    serial,
                                    cursor_style_to_icon_names(default_style),
                                    scale,
                                );
                            }
                        }
                    }
                    if state
                        .keyboard_focused_window
                        .as_ref()
                        .is_some_and(|keyboard_window| window.ptr_eq(keyboard_window))
                    {
                        state.enter_token = None;
                    }
                    let input = PlatformInput::MouseMove(MouseMoveEvent {
                        position: state.mouse_location.unwrap(),
                        pressed_button: state.button_pressed,
                        modifiers: state.modifiers,
                    });
                    drop(state);
                    window.handle_input(input);
                }
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                // Record presses only. Requests referencing this serial (popup grabs,
                // interactive moves) are declined when given a release serial.
                if button_state == wl_pointer::ButtonState::Pressed {
                    state.serial_tracker.update(SerialKind::MousePress, serial);
                }
                let button = linux_button_to_gpui(button);
                let Some(button) = button else { return };
                if state.mouse_focused_window.is_none() {
                    return;
                }
                match button_state {
                    wl_pointer::ButtonState::Pressed => {
                        if let Some(window) = state.keyboard_focused_window.clone() {
                            if state.composing && state.text_input.is_some() {
                                drop(state);
                                // text_input_v3 don't have something like a reset function
                                this.disable_ime();
                                this.enable_ime();
                                window.handle_ime(ImeInput::UnmarkText);
                                state = client.borrow_mut();
                            } else if let (Some(text), Some(compose)) =
                                (state.pre_edit_text.take(), state.compose_state.as_mut())
                            {
                                compose.reset();
                                drop(state);
                                window.handle_ime(ImeInput::InsertText(text));
                                state = client.borrow_mut();
                            }
                        }
                        let click_elapsed = state.click.last_click.elapsed();

                        if click_elapsed < DOUBLE_CLICK_INTERVAL
                            && state
                                .click
                                .last_mouse_button
                                .is_some_and(|prev_button| prev_button == button)
                            && is_within_click_distance(
                                state.click.last_location,
                                state.mouse_location.unwrap(),
                            )
                        {
                            state.click.current_count += 1;
                        } else {
                            state.click.current_count = 1;
                        }

                        state.click.last_click = Instant::now();
                        state.click.last_mouse_button = Some(button);
                        state.click.last_location = state.mouse_location.unwrap();

                        state.button_pressed = Some(button);

                        if let Some(window) = state.mouse_focused_window.clone() {
                            let input = PlatformInput::MouseDown(MouseDownEvent {
                                button,
                                position: state.mouse_location.unwrap(),
                                modifiers: state.modifiers,
                                click_count: state.click.current_count,
                                first_mouse: state.enter_token.take().is_some(),
                            });
                            drop(state);
                            window.handle_input(input);
                        }
                    }
                    wl_pointer::ButtonState::Released => {
                        state.button_pressed = None;

                        if let Some(window) = state.mouse_focused_window.clone() {
                            let input = PlatformInput::MouseUp(MouseUpEvent {
                                button,
                                position: state.mouse_location.unwrap(),
                                modifiers: state.modifiers,
                                click_count: state.click.current_count,
                            });
                            drop(state);
                            window.handle_input(input);
                        }
                    }
                    _ => {}
                }
            }

            // Axis Events
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(axis_source),
            } => {
                state.axis_source = axis_source;
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => {
                if state.axis_source == AxisSource::Wheel {
                    return;
                }
                let axis = if state.modifiers.shift {
                    wl_pointer::Axis::HorizontalScroll
                } else {
                    axis
                };
                let axis_modifier = match axis {
                    wl_pointer::Axis::VerticalScroll => state.vertical_modifier,
                    wl_pointer::Axis::HorizontalScroll => state.horizontal_modifier,
                    _ => 1.0,
                };
                state.scroll_event_received = true;
                let scroll_delta = state
                    .continuous_scroll_delta
                    .get_or_insert(point(px(0.0), px(0.0)));
                let modifier = 3.0;
                match axis {
                    wl_pointer::Axis::VerticalScroll => {
                        scroll_delta.y += px(value as f32 * modifier * axis_modifier);
                    }
                    wl_pointer::Axis::HorizontalScroll => {
                        scroll_delta.x += px(value as f32 * modifier * axis_modifier);
                    }
                    _ => unreachable!(),
                }
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => {
                state.scroll_event_received = true;
                let axis = if state.modifiers.shift {
                    wl_pointer::Axis::HorizontalScroll
                } else {
                    axis
                };
                let axis_modifier = match axis {
                    wl_pointer::Axis::VerticalScroll => state.vertical_modifier,
                    wl_pointer::Axis::HorizontalScroll => state.horizontal_modifier,
                    _ => 1.0,
                };

                let scroll_delta = state.discrete_scroll_delta.get_or_insert(point(0.0, 0.0));
                match axis {
                    wl_pointer::Axis::VerticalScroll => {
                        scroll_delta.y += discrete as f32 * axis_modifier * SCROLL_LINES;
                    }
                    wl_pointer::Axis::HorizontalScroll => {
                        scroll_delta.x += discrete as f32 * axis_modifier * SCROLL_LINES;
                    }
                    _ => unreachable!(),
                }
            }
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => {
                state.scroll_event_received = true;
                let axis = if state.modifiers.shift {
                    wl_pointer::Axis::HorizontalScroll
                } else {
                    axis
                };
                let axis_modifier = match axis {
                    wl_pointer::Axis::VerticalScroll => state.vertical_modifier,
                    wl_pointer::Axis::HorizontalScroll => state.horizontal_modifier,
                    _ => unreachable!(),
                };

                let scroll_delta = state.discrete_scroll_delta.get_or_insert(point(0.0, 0.0));
                let wheel_percent = value120 as f32 / 120.0;
                match axis {
                    wl_pointer::Axis::VerticalScroll => {
                        scroll_delta.y += wheel_percent * axis_modifier * SCROLL_LINES;
                    }
                    wl_pointer::Axis::HorizontalScroll => {
                        scroll_delta.x += wheel_percent * axis_modifier * SCROLL_LINES;
                    }
                    _ => unreachable!(),
                }
            }
            wl_pointer::Event::Frame => {
                if state.scroll_event_received {
                    state.scroll_event_received = false;
                    let continuous = state.continuous_scroll_delta.take();
                    let discrete = state.discrete_scroll_delta.take();
                    if let Some(continuous) = continuous {
                        if let Some(window) = state.mouse_focused_window.clone() {
                            let input = PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position: state.mouse_location.unwrap(),
                                delta: ScrollDelta::Pixels(continuous),
                                modifiers: state.modifiers,
                                touch_phase: TouchPhase::Moved,
                            });
                            drop(state);
                            window.handle_input(input);
                        }
                    } else if let Some(discrete) = discrete
                        && let Some(window) = state.mouse_focused_window.clone()
                    {
                        let input = PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position: state.mouse_location.unwrap(),
                            delta: ScrollDelta::Lines(discrete),
                            modifiers: state.modifiers,
                            touch_phase: TouchPhase::Moved,
                        });
                        drop(state);
                        window.handle_input(input);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_pointer_gestures_v1::ZwpPointerGesturesV1, ()> for WaylandClientStatePtr {
    fn event(
        _this: &mut Self,
        _: &zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
        _: <zwp_pointer_gestures_v1::ZwpPointerGesturesV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The gesture manager doesn't generate events
    }
}

impl Dispatch<zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1, ()>
    for WaylandClientStatePtr
{
    fn event(
        this: &mut Self,
        _: &zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1,
        event: <zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use gpui::PinchEvent;

        let client = this.get_client();
        let mut state = client.borrow_mut();

        let Some(window) = state.mouse_focused_window.clone() else {
            return;
        };

        match event {
            zwp_pointer_gesture_pinch_v1::Event::Begin {
                serial: _,
                time: _,
                surface: _,
                fingers: _,
            } => {
                state.pinch_scale = 1.0;
                let input = PlatformInput::Pinch(PinchEvent {
                    position: state.mouse_location.unwrap_or(point(px(0.0), px(0.0))),
                    delta: 0.0,
                    modifiers: state.modifiers,
                    phase: TouchPhase::Started,
                });
                drop(state);
                window.handle_input(input);
            }
            zwp_pointer_gesture_pinch_v1::Event::Update { time: _, scale, .. } => {
                let new_absolute_scale = scale as f32;
                let previous_scale = state.pinch_scale;
                let zoom_delta = new_absolute_scale - previous_scale;
                state.pinch_scale = new_absolute_scale;

                let input = PlatformInput::Pinch(PinchEvent {
                    position: state.mouse_location.unwrap_or(point(px(0.0), px(0.0))),
                    delta: zoom_delta,
                    modifiers: state.modifiers,
                    phase: TouchPhase::Moved,
                });
                drop(state);
                window.handle_input(input);
            }
            zwp_pointer_gesture_pinch_v1::Event::End {
                serial: _,
                time: _,
                cancelled: _,
            } => {
                state.pinch_scale = 1.0;
                let input = PlatformInput::Pinch(PinchEvent {
                    position: state.mouse_location.unwrap_or(point(px(0.0), px(0.0))),
                    delta: 0.0,
                    modifiers: state.modifiers,
                    phase: TouchPhase::Ended,
                });
                drop(state);
                window.handle_input(input);
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ObjectId> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: <wp_fractional_scale_v1::WpFractionalScaleV1 as Proxy>::Event,
        surface_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        let Some(window) = get_window(&mut state, surface_id) else {
            return;
        };

        drop(state);
        window.handle_fractional_scale_event(event);
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ObjectId>
    for WaylandClientStatePtr
{
    fn event(
        this: &mut Self,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        surface_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();
        let Some(window) = get_window(&mut state, surface_id) else {
            return;
        };

        drop(state);
        window.handle_toplevel_decoration_event(event);
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        match event {
            // Clipboard
            wl_data_device::Event::DataOffer { id: data_offer } => {
                state.data_offers.push(DataOffer::new(data_offer));
                if state.data_offers.len() > 2 {
                    // At most we store a clipboard offer and a drag and drop offer.
                    state.data_offers.remove(0).inner.destroy();
                }
            }
            wl_data_device::Event::Selection { id: data_offer } => {
                if let Some(offer) = data_offer {
                    let offer = state
                        .data_offers
                        .iter()
                        .find(|wrapper| wrapper.inner.id() == offer.id());
                    let offer = offer.cloned();
                    state.clipboard.set_offer(offer);
                } else {
                    state.clipboard.set_offer(None);
                }
            }

            // Drag and drop
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id: data_offer,
            } => {
                state.serial_tracker.update(SerialKind::DataDevice, serial);
                if let Some(data_offer) = data_offer {
                    let Some(drag_window) = get_window(&mut state, &surface.id()) else {
                        return;
                    };

                    // Said again here, with the serial of this very entry. The
                    // mime types arrive before it, so what was said then carried
                    // the serial of some earlier drag and the compositor ignored
                    // it -- leaving the drag unaccepted, which is why nothing
                    // was ever negotiated but a copy.
                    data_offer.accept(serial, Some(FILE_LIST_MIME_TYPE.to_string()));

                    // Copy until something in the window says it is taking the
                    // files rather than reading them. Most drops only read a
                    // path, and a window that asked for a move by default would
                    // have the file deleted out from under whoever dropped it.
                    data_offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                    state.drag.takes_the_drag = false;

                    let pipe = Pipe::new().unwrap();
                    data_offer.receive(FILE_LIST_MIME_TYPE.to_string(), unsafe {
                        BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
                    });
                    let fd = pipe.read;
                    drop(pipe.write);

                    let read_task = state.common.background_executor.spawn(async {
                        let buffer = read_fd_with_timeout(fd, PIPE_READ_TIMEOUT)?;
                        let text = String::from_utf8(buffer)?;
                        anyhow::Ok(text)
                    });

                    let this = this.clone();
                    state
                        .common
                        .foreground_executor
                        .spawn(async move {
                            let file_list = match read_task.await {
                                Ok(list) => list,
                                Err(err) => {
                                    log::error!("error reading drag and drop pipe: {err:?}");
                                    return;
                                }
                            };

                            let paths: SmallVec<[_; 2]> = file_list
                                .lines()
                                .filter_map(|path| Url::parse(path).log_err())
                                .filter_map(|url| match url.to_file_path() {
                                    Ok(url) => Some(url),
                                    Err(()) => {
                                        log::error!("Failed turn {url:?} into a file path");
                                        None
                                    }
                                })
                                .collect();
                            let position = Point::new(x.into(), y.into());

                            // Prevent dropping text from other programs.
                            if paths.is_empty() {
                                data_offer.destroy();
                                return;
                            }

                            let input = PlatformInput::FileDrop(FileDropEvent::Entered {
                                position,
                                paths: gpui::ExternalPaths(paths),
                            });

                            let client = this.get_client();
                            let mut state = client.borrow_mut();
                            state.drag.data_offer = Some(data_offer);
                            state.drag.window = Some(drag_window.clone());
                            state.drag.position = position;

                            drop(state);
                            drag_window.handle_input(input);
                        })
                        .detach();
                }
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                let Some(drag_window) = state.drag.window.clone() else {
                    return;
                };
                let position = Point::new(x.into(), y.into());
                state.drag.position = position;
                // Asked afresh for this step: what is under the pointer changes
                // as it moves, and only some of what it passes over takes the
                // file. Whatever is under it now answers while handling the
                // event below, and the answer is given to the compositor once,
                // after -- setting it before and again during would leave the
                // last word to whichever happened to be last.
                state.drag.takes_the_drag = false;
                let offer = state.drag.data_offer.clone();

                let input = PlatformInput::FileDrop(FileDropEvent::Pending { position });
                drop(state);
                drag_window.handle_input(input);

                let client = this.get_client();
                let state = client.borrow();
                if let Some(offer) = offer.as_ref()
                    && offer.version() >= 3
                {
                    let preferred = match state.drag.takes_the_drag {
                        true => DndAction::Move,
                        false => DndAction::Copy,
                    };
                    offer.set_actions(DndAction::Copy | DndAction::Move, preferred);
                }
            }
            wl_data_device::Event::Leave => {
                let Some(drag_window) = state.drag.window.clone() else {
                    return;
                };
                let data_offer = state.drag.data_offer.clone().unwrap();
                data_offer.destroy();

                state.drag.data_offer = None;
                state.drag.window = None;

                let input = PlatformInput::FileDrop(FileDropEvent::Exited {});
                drop(state);
                drag_window.handle_input(input);
            }
            wl_data_device::Event::Drop => {
                let Some(drag_window) = state.drag.window.clone() else {
                    return;
                };
                let data_offer = state.drag.data_offer.clone().unwrap();
                log::info!(
                    "drop into this window: as {:?}; this window asked to take the files: {}",
                    state.drag.action,
                    state.drag.takes_the_drag
                );
                // Only a drag that was accepted and settled on an action may be
                // finished; saying so about any other is a protocol error, and a
                // protocol error takes the whole connection with it.
                if data_offer.version() >= 3 && !state.drag.action.is_empty() {
                    data_offer.finish();
                }
                data_offer.destroy();

                state.drag.data_offer = None;
                state.drag.window = None;

                let input = PlatformInput::FileDrop(FileDropEvent::Submit {
                    position: state.drag.position,
                });
                drop(state);
                drag_window.handle_input(input);
            }
            _ => {}
        }
    }

    event_created_child!(WaylandClientStatePtr, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        data_offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        if let wl_data_offer::Event::Action { dnd_action } = &event {
            let settled = dnd_action.into_result().unwrap_or(DndAction::empty());
            state.drag.action = settled;
            log::info!(
                "drag into this window: the desktop settled on {:?}",
                settled
            );
        }

        if let wl_data_offer::Event::Offer { mime_type } = event {
            // Drag and drop
            if mime_type == FILE_LIST_MIME_TYPE {
                let serial = state.serial_tracker.get(SerialKind::DataDevice);
                let mime_type = mime_type.clone();
                data_offer.accept(serial, Some(mime_type));
            }

            // Clipboard
            if let Some(offer) = state
                .data_offers
                .iter_mut()
                .find(|wrapper| wrapper.inner.id() == data_offer.id())
            {
                offer.add_mime_type(mime_type);
            }
        }
    }
}

/// Marks the `wl_data_source` of a drag this window started, so that what it is
/// asked to send is the dragged files rather than whatever is on the clipboard.
pub(crate) struct DraggedOut;

impl Dispatch<wl_data_source::WlDataSource, DraggedOut> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        data_source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &DraggedOut,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        match event {
            // What the receiver asked for. It is the only thing this window
            // ever learns about whatever is on the other end of the drag: the
            // protocol tells a source nothing about where its drag landed.
            wl_data_source::Event::Send { mime_type, fd } => {
                let paths = state.dragged_files.clone();
                log::info!(
                    "drag out of this window: the receiver asked for {mime_type} while the action stands at {:?}; answering with {} file(s): {:?}",
                    state.dragged_action,
                    paths.len(),
                    paths
                );
                state.clipboard.send_paths(&mime_type, &paths, fd);
            }
            // What the desktop settled on. It can change while the drag is in
            // flight -- the key that means "copy" is held or let go -- so the
            // last word before the drop is the one that counts.
            wl_data_source::Event::Action { dnd_action } => {
                state.dragged_action = dnd_action.into_result().unwrap_or(DndAction::empty());
                log::info!(
                    "drag out of this window: the desktop settled on {:?}",
                    state.dragged_action
                );
            }
            // The drop landed and whoever took it has finished reading. A move
            // is only half done at this point: the files have been copied to
            // wherever they went, and removing the originals is what makes it a
            // move rather than a copy. That is the source's to do, and the
            // source is this window.
            wl_data_source::Event::DndFinished => {
                let carried = std::mem::take(&mut state.dragged_files);
                let carried_count = carried.len();
                let paths = files_the_drag_gave_up(state.dragged_action, carried);
                let moving = !paths.is_empty();
                log::info!(
                    "drag out of this window finished as {:?}; {} file(s) carried, {}",
                    state.dragged_action,
                    carried_count,
                    match moving {
                        true => "let go of here",
                        false => "left where they are",
                    }
                );
                if let Some(icon) = state.drag_icon.take() {
                    icon.let_go();
                }
                if moving {
                    state
                        .common
                        .background_executor
                        .spawn(async move { let_go_of(&paths) })
                        .detach();
                }
                data_source.destroy();
            }
            // Nothing took it. Either it was dropped where nothing was
            // listening, or what it was dropped on could not do what the drag
            // asked for -- a window that only copies refuses a drag that only
            // moves, and there is no second chance to offer it a copy instead.
            wl_data_source::Event::Cancelled => {
                log::info!(
                    "drag out of this window was refused; {} file(s) stay where they are, \
                     and the action stood at {:?}",
                    state.dragged_files.len(),
                    state.dragged_action
                );
                state.dragged_files.clear();
                state.dragged_action = DndAction::empty();
                if let Some(icon) = state.drag_icon.take() {
                    icon.let_go();
                }
                data_source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for WaylandClientStatePtr {
    fn event(
        this: &mut Self,
        data_source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let state = client.borrow_mut();

        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                state.clipboard.send(mime_type, fd);
            }
            wl_data_source::Event::Cancelled => {
                data_source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, ()>
    for WaylandClientStatePtr
{
    fn event(
        this: &mut Self,
        _: &zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        match event {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                let old_offer = state.primary_data_offer.replace(DataOffer::new(offer));
                if let Some(old_offer) = old_offer {
                    old_offer.inner.destroy();
                }
            }
            zwp_primary_selection_device_v1::Event::Selection { id: data_offer } => {
                if data_offer.is_some() {
                    let offer = state.primary_data_offer.clone();
                    state.clipboard.set_primary_offer(offer);
                } else {
                    state.clipboard.set_primary_offer(None);
                }
            }
            _ => {}
        }
    }

    event_created_child!(WaylandClientStatePtr, zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()>
    for WaylandClientStatePtr
{
    fn event(
        this: &mut Self,
        _data_offer: &zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let mut state = client.borrow_mut();

        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event
            && let Some(offer) = state.primary_data_offer.as_mut()
        {
            offer.add_mime_type(mime_type);
        }
    }
}

impl Dispatch<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1, ()>
    for WaylandClientStatePtr
{
    fn event(
        this: &mut Self,
        selection_source: &zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let client = this.get_client();
        let state = client.borrow_mut();

        match event {
            zwp_primary_selection_source_v1::Event::Send { mime_type, fd } => {
                state.clipboard.send_primary(mime_type, fd);
            }
            zwp_primary_selection_source_v1::Event::Cancelled => {
                selection_source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgWmDialogV1, ()> for WaylandClientStatePtr {
    fn event(
        _: &mut Self,
        _: &XdgWmDialogV1,
        _: <XdgWmDialogV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgDialogV1, ()> for WaylandClientStatePtr {
    fn event(
        _state: &mut Self,
        _proxy: &XdgDialogV1,
        _event: <XdgDialogV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// What a drag offers decides what it can become, and it is decided once,
    /// before the drag starts. A move that also offers a copy is settled as a
    /// copy by any receiver that would rather copy -- which a file manager
    /// receiving a drag from another application does.
    #[test]
    fn a_drag_that_means_to_move_offers_nothing_else() {
        let moving = offered_by(gpui::DragMeans::Taking);

        assert!(moving.contains(DndAction::Move), "the move is on offer");
        assert!(
            !moving.contains(DndAction::Copy),
            "and the copy is not, or the receiver settles on it: {moving:?}"
        );
    }

    /// And a copy offers nothing else either, for the same reason read the other
    /// way round: a receiver free to settle on a move would have the original
    /// deleted after a drag the reader asked to be a copy.
    #[test]
    fn a_drag_that_means_to_copy_offers_nothing_else() {
        let copying = offered_by(gpui::DragMeans::Copying);

        assert!(copying.contains(DndAction::Copy), "the copy is on offer");
        assert!(
            !copying.contains(DndAction::Move),
            "and the move is not, or the original is taken from under the reader: {copying:?}"
        );
    }

    #[derive(Default)]
    struct FakeImeCursorRectangleSink {
        cursor_rectangles: RefCell<Vec<(i32, i32, i32, i32)>>,
        commit_count: Cell<usize>,
    }

    impl ImeCursorRectangleSink for FakeImeCursorRectangleSink {
        fn set_ime_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
            self.cursor_rectangles
                .borrow_mut()
                .push((x, y, width, height));
        }

        fn commit_ime_state(&self) {
            self.commit_count.set(self.commit_count.get() + 1);
        }
    }

    fn ime_cursor_bounds(x: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(20.25)), size(px(1.0), px(18.75)))
    }

    #[test]
    fn caches_cursor_rectangle_committed_after_done() {
        let text_input = FakeImeCursorRectangleSink::default();
        let mut last_ime_cursor_rectangle = None;
        let initial_bounds = ime_cursor_bounds(10.0);
        let updated_bounds = ime_cursor_bounds(20.0);

        update_ime_cursor_rectangle(&text_input, &mut last_ime_cursor_rectangle, initial_bounds);
        set_ime_cursor_rectangle_after_done(
            &text_input,
            &mut last_ime_cursor_rectangle,
            updated_bounds,
            true,
        );
        update_ime_cursor_rectangle(&text_input, &mut last_ime_cursor_rectangle, updated_bounds);

        assert_eq!(text_input.commit_count.get(), 2);
        assert_eq!(text_input.cursor_rectangles.borrow().len(), 2);
    }

    #[test]
    fn skips_unchanged_cursor_rectangle_after_done() {
        let text_input = FakeImeCursorRectangleSink::default();
        let mut last_ime_cursor_rectangle = None;
        let bounds = ime_cursor_bounds(10.0);

        update_ime_cursor_rectangle(&text_input, &mut last_ime_cursor_rectangle, bounds);
        set_ime_cursor_rectangle_after_done(
            &text_input,
            &mut last_ime_cursor_rectangle,
            bounds,
            true,
        );

        assert_eq!(text_input.commit_count.get(), 1);
        assert_eq!(text_input.cursor_rectangles.borrow().len(), 1);
    }

    #[test]
    fn skips_cursor_rectangles_with_unchanged_protocol_coordinates() {
        let text_input = FakeImeCursorRectangleSink::default();
        let mut last_ime_cursor_rectangle = None;

        update_ime_cursor_rectangle(
            &text_input,
            &mut last_ime_cursor_rectangle,
            ime_cursor_bounds(10.25),
        );
        update_ime_cursor_rectangle(
            &text_input,
            &mut last_ime_cursor_rectangle,
            ime_cursor_bounds(10.75),
        );

        assert_eq!(text_input.commit_count.get(), 1);
        assert_eq!(
            text_input.cursor_rectangles.borrow().as_slice(),
            &[(10, 20, 1, 18)]
        );
    }

    fn a_file(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"something").expect("the file is written");
        path
    }

    /// A drag the desktop settled on moving is only half done when the drop
    /// lands: the files have been copied to wherever they went, and letting go
    /// of the originals is what makes it a move rather than a copy.
    #[test]
    fn a_drag_taken_as_a_move_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("a directory of our own");
        let one = a_file(dir.path(), "one.txt");
        let two = a_file(dir.path(), "two.txt");

        let given_up = files_the_drag_gave_up(DndAction::Move, vec![one.clone(), two.clone()]);
        assert_eq!(given_up, vec![one.clone(), two.clone()]);
        let_go_of(&given_up);

        assert!(!one.exists(), "a file that was moved is not still here");
        assert!(!two.exists(), "nor is the second one");
    }

    /// A copy leaves both: the one that was taken and the one it was taken from.
    #[test]
    fn a_drag_taken_as_a_copy_leaves_everything_where_it_was() {
        let dir = tempfile::tempdir().expect("a directory of our own");
        let one = a_file(dir.path(), "one.txt");

        let given_up = files_the_drag_gave_up(DndAction::Copy, vec![one.clone()]);
        assert!(given_up.is_empty(), "a copy gives nothing up");
        let_go_of(&given_up);

        assert!(one.exists(), "the file that was copied is still here");
    }

    /// Nothing at all was agreed -- the drag was let go over something that
    /// would not take it -- and nothing is removed.
    #[test]
    fn a_drag_nobody_took_leaves_everything_where_it_was() {
        let dir = tempfile::tempdir().expect("a directory of our own");
        let one = a_file(dir.path(), "one.txt");

        assert!(files_the_drag_gave_up(DndAction::empty(), vec![one.clone()]).is_empty());
        assert!(one.exists());
    }

    /// A file already gone by the time the drop finishes -- whoever took it
    /// moved it themselves -- is not an error to report.
    #[test]
    fn a_file_already_gone_is_not_a_complaint() {
        let dir = tempfile::tempdir().expect("a directory of our own");
        let gone = dir.path().join("never-was.txt");

        let_go_of(std::slice::from_ref(&gone));

        assert!(!gone.exists());
    }

    /// The picture says how many files are being carried: one page for one, and
    /// a second behind it for more.
    #[test]
    fn the_picture_says_how_many_files_are_carried() {
        let one = drag_icon_pixels(1);
        let several = drag_icon_pixels(4);

        assert_eq!(one.len(), (DRAG_ICON_SIZE * DRAG_ICON_SIZE * 4) as usize);
        assert_eq!(one.len(), several.len());
        assert_ne!(one, several, "a stack of files does not look like one file");
    }
}
