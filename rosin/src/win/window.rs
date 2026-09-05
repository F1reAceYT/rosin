//! Win32 windowing backend.
//!
//! Mirrors the responsibilities of `mac/window.rs`, but talks to raw Win32 (via `windows-rs`)
//! instead of AppKit, and uses the Windows Pointer Input API (`WM_POINTER*` + `GetPointerPenInfo`)
//! instead of `NSEvent` so that stylus pressure/tilt/twist survive the trip into
//! `rosin_core::pointer::PointerEvent`.
//!
//! v1 scope: window creation, resize, pointer/keyboard input, animation-timer driven redraw,
//! and the wgpu+vello render path. Not yet wired: AccessKit (`accesskit_windows`), native menus,
//! file dialogs, and hot-reload -- each has a `TODO` at its call site in this file or in
//! `win/handle.rs`. Text input (plain `WM_CHAR` + IMM32 IME composition) lives in `win/ime.rs`.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    rc::Rc,
    sync::OnceLock,
    time::{Duration, Instant},
};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, ScreenToClient, ValidateRect, BLACK_BRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CAPITAL, VK_CONTROL, VK_LWIN, VK_MENU, VK_NUMLOCK, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Input::Pointer::{
    EnableMouseInPointer, GetPointerInfo, GetPointerPenInfo, POINTER_FLAG_FIRSTBUTTON, POINTER_FLAG_SECONDBUTTON,
    POINTER_INFO, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect,
    LoadCursorW, PostQuitMessage, RegisterClassExW, SetTimer, SetWindowLongPtrW, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, DLGC_WANTARROWS, DLGC_WANTCHARS, DLGC_WANTTAB,
    GWLP_USERDATA, HCURSOR, IDC_ARROW,
    IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZEWE, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZENESW,
    PT_PEN, SW_SHOW, WM_CHAR, WM_CLOSE, WM_CREATE,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETDLGCODE, WM_GETMINMAXINFO, WM_IME_COMPOSITION,
    WM_IME_ENDCOMPOSITION, WM_IME_SETCONTEXT, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT,
    WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE, WM_SETCURSOR, WM_SIZE, WM_TIMER, WM_UNICHAR, WNDCLASSEXW, WS_EX_NOREDIRECTIONBITMAP,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_SIZEBOX,
};

use super::{ime, util};
use crate::{
    handle::WindowHandle,
    keyboard_types,
    kurbo::{Point as KPoint, Size, Vec2},
    log::error,
    peniko,
    pointer::{PointerButton, PointerButtons, PointerEvent as RosinPointerEvent, PointerType},
    prelude::*,
    vello, wgpu,
    win::handle::WindowHandle as WinWindowHandle,
};
use rosin_core::viewport::*;

/// Posted by [`WindowHandle::create_window`] to ask the owning window to spawn a child window
/// on the UI thread (Win32 windows must be created on the thread that will pump their messages).
pub(crate) const WM_ROSIN_CREATE_WINDOW: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

/// lParam bit in `WM_IME_SETCONTEXT` that tells the IME to draw its floating composition
/// (pre-edit) window. Rosin draws marked text itself, so we strip it before defprocing.
const IS_SHOWUICOMPOSITIONWINDOW: u32 = 0x8000_0000;
pub(crate) const TIMER_EVENT_ID: usize = 1;

const WINDOW_CLASS_NAME: PCWSTR = windows::core::w!("RosinWindowClass");

/// Per-window state stashed in `GWLP_USERDATA`. Analogous to `mac::window::ViewIvars`, minus
/// the AppKit-specific fields (display link, tracking area, a11y adapter -- see module docs).
pub(crate) struct WindowExtra {
    pub(crate) viewport: RefCell<Box<dyn WinViewportTrait>>,
    pub(crate) gpu_ctx: Rc<GpuCtx>,
    pub(crate) vello_renderer: Rc<RefCell<vello::Renderer>>,
    pub(crate) vello_texture: RefCell<wgpu::Texture>,
    pub(crate) surface: RefCell<Option<wgpu::Surface<'static>>>,
    pub(crate) surface_format: Cell<Option<wgpu::TextureFormat>>,
    pub(crate) needs_config: Cell<bool>,
    pub(crate) last_frame: Cell<Option<Instant>>,
    pub(crate) window_state: Cell<WindowState>,
    pub(crate) min_size: Cell<Option<Size>>,
    pub(crate) max_size: Cell<Option<Size>>,
    pub(crate) cursor_hidden: Cell<bool>,
    pub(crate) input_handler: RefCell<Option<Box<dyn InputHandler + Send + Sync>>>,
    pub(crate) input_handler_node: Cell<Option<NodeId>>,
    // Holds the first half of a surrogate pair between the two WM_CHAR messages
    // TranslateMessage emits for characters outside the BMP.
    pending_high_surrogate: Cell<Option<u16>>,
    pub(crate) pending_child_windows: RefCell<Vec<Box<dyn Any + Send + Sync>>>,
    next_timer_id: Cell<usize>,
}

impl WindowExtra {
    pub(crate) fn next_timer_id(&self, _node: Option<NodeId>) -> usize {
        let id = self.next_timer_id.get().max(TIMER_EVENT_ID + 1);
        self.next_timer_id.set(id + 1);
        id
    }
}

/// Windows analogue of `mac::window::ViewportTrait`. Kept separate (rather than shared) because
/// the mac trait's methods take `&NSEvent`, which has no meaning here -- see the open question
/// in the accompanying chat message about whether to unify these behind a platform-agnostic
/// event type in `rosin_core` once both backends exist.
pub(crate) trait WinViewportTrait {
    fn init_viewport(&mut self, hwnd: HWND);
    fn dispatch_and_redraw(&mut self, hwnd: HWND);
    fn pointer_event(&mut self, hwnd: HWND, event: RosinPointerEvent, kind: PointerEventKind);
    fn wheel_event(&mut self, hwnd: HWND, event: RosinPointerEvent);
    fn keyboard_event(&mut self, hwnd: HWND, vk: u32, down: bool, repeat: bool);
    fn render(&mut self, hwnd: HWND, extra: &WindowExtra);
    fn set_size(&mut self, size: Size);
    fn is_idle(&self) -> bool;
    fn animation_frame(&mut self, dt: Duration);
    fn queue_file_dialog_event(&mut self, node: NodeId, response: crate::events::FileDialogResponse);
    fn queue_command_event(&mut self, node: Option<NodeId>, command: CommandId);
    fn change_event(&mut self, node: NodeId);
    fn translation_map(&self) -> TranslationMap;
}

#[derive(PartialEq, Eq)]
pub(crate) enum PointerEventKind {
    Down,
    Move,
    Up,
}

/// Wraps a generic `Viewport<S, WindowHandle>` + app state so it can live behind
/// `Box<dyn WinViewportTrait>` in `GWLP_USERDATA`, which cannot itself be generic.
struct ViewportContainer<S: Sync + 'static> {
    desc: WindowDesc<S>,
    app_state: Rc<RefCell<S>>,
    viewport: Viewport<S, WindowHandle>,
}

impl<S: Sync + 'static> WinViewportTrait for ViewportContainer<S> {
    fn init_viewport(&mut self, hwnd: HWND) {
        let platform_handle = WinWindowHandle::new(hwnd);
        let handle = WindowHandle(platform_handle);
        let mut state = self.app_state.borrow_mut();
        let _ = self.viewport.frame(&state);
        self.viewport.dispatch_event_queue(&mut state, &handle);
    }

    fn dispatch_and_redraw(&mut self, hwnd: HWND) {
        let platform_handle = WinWindowHandle::new(hwnd);
        let handle = WindowHandle(platform_handle);
        let mut state = self.app_state.borrow_mut();
        self.viewport.dispatch_event_queue(&mut state, &handle);
        if !self.viewport.is_idle() {
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

    fn pointer_event(&mut self, _hwnd: HWND, event: RosinPointerEvent, kind: PointerEventKind) {
        match kind {
            PointerEventKind::Down => self.viewport.queue_pointer_down_event(&event),
            PointerEventKind::Move => self.viewport.queue_pointer_move_event(&event),
            PointerEventKind::Up => self.viewport.queue_pointer_up_event(&event),
        }
    }

    fn wheel_event(&mut self, _hwnd: HWND, event: RosinPointerEvent) {
        self.viewport.queue_pointer_wheel_event(&event);
    }

    fn keyboard_event(&mut self, hwnd: HWND, vk: u32, down: bool, repeat: bool) {
        // Imported by mac/util.rs too: mark events as composing while the window's
        // text input handler has an open composition so widgets can skip echo text.
        let is_composing = with_extra(hwnd, |extra| {
            extra.input_handler.borrow().as_deref().and_then(|h| h.composition_range().map(|r| !r.is_empty()))
        })
        .flatten()
        .unwrap_or(false);

        let event = util::convert_key_event(vk, down, repeat, current_modifiers(), is_composing);
        self.viewport.queue_keyboard_event(&event);
    }

    fn set_size(&mut self, size: Size) {
        self.viewport.set_size(size);
    }

    fn is_idle(&self) -> bool {
        self.viewport.is_idle()
    }

    fn animation_frame(&mut self, dt: Duration) {
        self.viewport.queue_animation_events(dt);
    }

    fn queue_file_dialog_event(&mut self, node: NodeId, response: crate::events::FileDialogResponse) {
        self.viewport.queue_file_dialog_event(node, response);
    }

    fn queue_command_event(&mut self, node: Option<NodeId>, command: CommandId) {
        self.viewport.queue_command_event(node, command);
    }

    fn change_event(&mut self, node: NodeId) {
        self.viewport.queue_change_event(node);
    }

    fn translation_map(&self) -> TranslationMap {
        self.viewport.get_translation_map()
    }

    fn render(&mut self, hwnd: HWND, extra: &WindowExtra) {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rect);
        }
        let physical_size = Size::new((rect.right - rect.left) as f64, (rect.bottom - rect.top) as f64);
        if physical_size.width == 0.0 || physical_size.height == 0.0 {
            return;
        }

        let gpu_ctx = &extra.gpu_ctx;

        if extra.surface.borrow().is_none() {
            let handle = WinWindowHandle::new(hwnd);
            match gpu_ctx.instance.create_surface(handle) {
                Ok(surface) => *extra.surface.borrow_mut() = Some(surface),
                Err(e) => {
                    error!("Failed to create wgpu surface: {e:?}");
                    return;
                }
            }
        }

        let surface_ref = extra.surface.borrow();
        let Some(surface) = surface_ref.as_ref() else { return };

        if extra.needs_config.get() {
            let capabilities = surface.get_capabilities(&gpu_ctx.adapter);
            let Some(format) = capabilities
                .formats
                .into_iter()
                .find(|f| matches!(f, wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm))
            else {
                error!("Surface doesn't support Rgba8Unorm or Bgra8Unorm");
                return;
            };

            surface.configure(
                &gpu_ctx.device,
                &wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format,
                    width: physical_size.width as u32,
                    height: physical_size.height as u32,
                    // DXGI's own flip-model swap chain already paces to the display; AutoVsync
                    // lets wgpu pick the DXGI present interval instead of a fixed choice.
                    present_mode: wgpu::PresentMode::AutoVsync,
                    alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                    view_formats: vec![],
                    desired_maximum_frame_latency: 2,
                },
            );
            extra.surface_format.set(Some(format));

            *extra.vello_texture.borrow_mut() = gpu_ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: physical_size.width as u32,
                    height: physical_size.height as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            extra.needs_config.set(false);
        }

        let surface_texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex) | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => tex,
            _ => {
                extra.needs_config.set(true);
                return;
            }
        };

        let mut state = self.app_state.borrow_mut();
        let scene = self.viewport.frame(&state);
        let begin_paint = Instant::now();

        if let Some(wgpufn) = self.desc.wgpufn {
            let target = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = gpu_ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Callback Encoder") });
            let mut render_ctx = WgpuCtx {
                device: &gpu_ctx.device,
                queue: &gpu_ctx.queue,
                target: &target,
                target_format: surface_texture.texture.format(),
                encoder: &mut encoder,
            };
            (wgpufn.func)(&state, &mut render_ctx);
            gpu_ctx.queue.submit(Some(encoder.finish()));
        }

        let vello_texture_view = extra.vello_texture.borrow().create_view(&wgpu::TextureViewDescriptor {
            label: None,
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            dimension: Some(wgpu::TextureViewDimension::D2),
            aspect: wgpu::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: Some(1),
            base_array_layer: 0,
            array_layer_count: Some(1),
            usage: None,
        });

        let params = vello::RenderParams {
            base_color: peniko::Color::TRANSPARENT,
            width: physical_size.width as u32,
            height: physical_size.height as u32,
            antialiasing_method: vello::AaConfig::Msaa16,
        };

        if let Err(e) = extra
            .vello_renderer
            .borrow_mut()
            .render_to_texture(&gpu_ctx.device, &gpu_ctx.queue, scene, &vello_texture_view, &params)
        {
            error!("Failed to render to texture: {e:?}");
            extra.needs_config.set(true);
            return;
        }

        let surface_texture_view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu_ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Compositing Pass") });

        // NOTE: this whole block, from `frame()` through the blit below, is byte-for-byte
        // identical to the tail of mac::window::update_layer -- it's pure wgpu/vello, nothing
        // AppKit-specific. Worth extracting into a shared `gpu::render_frame()` helper once both
        // backends exist, so the two platforms can't silently drift.
        let mut compositor = gpu_ctx.compositor.blitter.borrow_mut();
        let compositor = compositor.get_or_insert_with(|| wgpu::util::TextureBlitter::new(&gpu_ctx.device, wgpu::TextureFormat::Bgra8Unorm));
        compositor.copy(&gpu_ctx.device, &mut encoder, &vello_texture_view, &surface_texture_view);

        gpu_ctx.queue.submit(Some(encoder.finish()));
        surface_texture.present();
        self.viewport.report_paint_time(Instant::now().duration_since(begin_paint));
        let platform_handle = WinWindowHandle::new(hwnd);
        self.viewport.dispatch_event_queue(&mut state, &WindowHandle(platform_handle));

        // TODO: accesskit_windows adapter update, mirroring the a11y_adapter.update_if_active
        // call at the end of mac::window::update_layer.
    }
}

/// Runs the closure with a reference to this window's [`WindowExtra`], if it's been set up.
/// Returns `None` if called before `WM_CREATE` finishes or after `WM_NCDESTROY` runs.
pub(crate) fn with_extra<R>(hwnd: HWND, f: impl FnOnce(&WindowExtra) -> R) -> Option<R> {
    unsafe {
        let ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowExtra;
        if ptr.is_null() {
            return None;
        }
        Some(f(&*ptr))
    }
}

pub(crate) fn set_cursor(cursor: CursorType) {
    let id = match cursor {
        CursorType::Default => IDC_ARROW,
        CursorType::ContextMenu => IDC_ARROW, // No direct equivalent, fall back to arrow
        CursorType::Help => IDC_HELP,
        CursorType::Pointer => IDC_HAND,
        CursorType::Crosshair => IDC_CROSS,
        CursorType::Text => IDC_IBEAM,
        CursorType::VerticalText => IDC_IBEAM,
        CursorType::Cell => IDC_CROSS, // No direct equivalent, crosshair is closest
        CursorType::Alias => IDC_ARROW, // No direct equivalent
        CursorType::Copy => IDC_ARROW, // No direct equivalent
        CursorType::Move | CursorType::Grab => IDC_SIZEALL,
        CursorType::NotAllowed => IDC_NO,
        CursorType::Grabbing => IDC_SIZEALL,
        CursorType::ColResize | CursorType::EWResize => IDC_SIZEWE,
        CursorType::RowResize | CursorType::NSResize => IDC_SIZENS,
        CursorType::NResize => IDC_SIZENS,
        CursorType::SResize => IDC_SIZENS,
        CursorType::EResize => IDC_SIZEWE,
        CursorType::WResize => IDC_SIZEWE,
        CursorType::NEResize | CursorType::SEResize => IDC_SIZENESW,
        CursorType::NWResize | CursorType::SWResize => IDC_SIZENWSE,
        CursorType::NESWResize => IDC_SIZENESW,
        CursorType::NWSEResize => IDC_SIZENWSE,
        CursorType::ZoomIn => IDC_ARROW, // No direct equivalent
        CursorType::ZoomOut => IDC_ARROW, // No direct equivalent
    };
    unsafe {
        if let Ok(hcursor) = LoadCursorW(None, id) {
            windows::Win32::UI::WindowsAndMessaging::SetCursor(Some(hcursor));
        }
    }
}

fn register_class_once() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| unsafe {
        let hinstance = GetModuleHandleW(None).expect("GetModuleHandleW failed");
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR::default()),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: WINDOW_CLASS_NAME,
            ..Default::default()
        };
        let atom = RegisterClassExW(&class);
        assert!(atom != 0, "RegisterClassExW failed");

        // Route pointer input exclusively through WM_POINTER* (pressure/tilt/twist-capable)
        // instead of also receiving the legacy WM_LBUTTONDOWN-style translation, which would
        // otherwise double up input handling between the two message families.
        let _ = EnableMouseInPointer(true);
    });
}

pub(crate) fn create_window<S: Sync + 'static>(
    desc: &WindowDesc<S>,
    state: Rc<RefCell<S>>,
    gpu_ctx: Rc<GpuCtx>,
    vello_renderer: Rc<RefCell<vello::Renderer>>,
) -> HWND {
    register_class_once();

    let mut style = WS_OVERLAPPEDWINDOW;
    if !desc.resizeable {
        style &= !(WS_SIZEBOX | WS_MAXIMIZEBOX);
    }
    if !desc.minimize_button {
        style &= !WS_MINIMIZEBOX;
    }

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: desc.size.width as i32,
        bottom: desc.size.height as i32,
    };
    unsafe {
        let _ = AdjustWindowRectEx(&mut rect, style, false, Default::default());
    }

    let (x, y) = desc
        .position
        .map(|p| (p.x as i32, p.y as i32))
        .unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT));

    let mut title_w: Vec<u16> = desc.title.as_deref().unwrap_or("").encode_utf16().collect();
    title_w.push(0);

    let scale = 1.0; // Corrected against the real DPI in WM_CREATE / WM_DPICHANGED once the HWND exists.
    let viewport = Viewport::new(desc.viewfn.func, desc.size, Vec2::new(scale, scale), TranslationMap::default());

    let container: Box<dyn WinViewportTrait> = Box::new(ViewportContainer {
        desc: desc.clone(),
        app_state: state,
        viewport,
    });

    let vello_texture = gpu_ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: desc.size.width as u32, height: desc.size.height as u32, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });

    let extra = Box::new(WindowExtra {
        viewport: RefCell::new(container),
        gpu_ctx,
        vello_renderer,
        vello_texture: RefCell::new(vello_texture),
        surface: RefCell::new(None),
        surface_format: Cell::new(None),
        needs_config: Cell::new(true),
        last_frame: Cell::new(None),
        window_state: Cell::new(WindowState::Normal),
        min_size: Cell::new(desc.min_size),
        max_size: Cell::new(desc.max_size),
        cursor_hidden: Cell::new(false),
        input_handler: RefCell::new(None),
        input_handler_node: Cell::new(None),
        pending_high_surrogate: Cell::new(None),
        pending_child_windows: RefCell::new(Vec::new()),
        next_timer_id: Cell::new(TIMER_EVENT_ID),
    });
    // Passed through CREATESTRUCT.lpCreateParams and installed on GWLP_USERDATA in WM_CREATE.
    let extra_ptr = Box::into_raw(extra);

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP, // wgpu owns presentation via DXGI; DWM shouldn't allocate a redirection surface too.
            WINDOW_CLASS_NAME,
            PCWSTR(title_w.as_ptr()),
            style,
            x,
            y,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(GetModuleHandleW(None).unwrap_or_default().into()),
            Some(extra_ptr as *const _),
        )
    }
    .expect("CreateWindowExW failed");

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // 144Hz-capable redraw pacing: WM_PAINT alone is display-driven and can stall under
        // heavy scrub load, so a short timer nudges a redraw the same way the mac backend's
        // CADisplayLink does. Revisit if this isn't tight enough under real stylus load --
        // a dedicated render thread signaled by the input thread is the next step up.
        SetTimer(Some(hwnd), TIMER_EVENT_ID, 1000 / 144, None);
    }

    with_extra(hwnd, |extra| extra.viewport.borrow_mut().init_viewport(hwnd));

    hwnd
}

fn pointer_event_from_msg(hwnd: HWND, wparam: WPARAM) -> Option<RosinPointerEvent> {
    // GET_POINTERID_WPARAM isn't surfaced by windows-rs 0.61; it's just LOWORD(wParam).
    let pointer_id = (wparam.0 & 0xFFFF) as u32;

    let mut info = POINTER_INFO::default();
    unsafe {
        GetPointerInfo(pointer_id, &mut info).ok()?;
    }

    let mut client_pt = POINT { x: info.ptPixelLocation.x, y: info.ptPixelLocation.y };
    unsafe {
        let _ = ScreenToClient(hwnd, &mut client_pt);
    }

    let button = if info.pointerFlags.0 & POINTER_FLAG_FIRSTBUTTON.0 != 0 {
        PointerButton::Primary
    } else if info.pointerFlags.0 & POINTER_FLAG_SECONDBUTTON.0 != 0 {
        PointerButton::Secondary
    } else {
        PointerButton::None
    };
    let mut buttons = PointerButtons::empty();
    if button != PointerButton::None {
        buttons.insert(button);
    }

    let mut event = RosinPointerEvent {
        viewport_pos: KPoint::new(client_pt.x as f64, client_pt.y as f64),
        button,
        buttons,
        mods: current_modifiers(),
        pointer_type: PointerType::Mouse,
        pressure: 1.0,
        ..RosinPointerEvent::default()
    };

    if info.pointerType == PT_PEN {
        let mut pen_info = POINTER_PEN_INFO::default();
        if unsafe { GetPointerPenInfo(pointer_id, &mut pen_info) }.is_ok() {
            event.pointer_type = PointerType::Pen;
            // pressure is 0..=1024 per the Pointer Input API; normalize to 0.0..=1.0.
            event.pressure = pen_info.pressure as f32 / 1024.0;
            event.tilt = Vec2::new(pen_info.tiltX as f64 / 90.0, pen_info.tiltY as f64 / 90.0);
            event.twist = pen_info.rotation as f32;
        }
    }

    Some(event)
}

pub(crate) fn current_modifiers() -> keyboard_types::Modifiers {
    let mut mods = keyboard_types::Modifiers::empty();
    unsafe {
        if GetKeyState(VK_CONTROL.0 as i32) < 0 {
            mods.insert(keyboard_types::Modifiers::CONTROL);
        }
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            mods.insert(keyboard_types::Modifiers::SHIFT);
        }
        if GetKeyState(VK_MENU.0 as i32) < 0 {
            mods.insert(keyboard_types::Modifiers::ALT);
        }
        // Win/Command keys: must poll the side-specific codes; VK_LWIN/VK_RWIN only
        // report to GetKeyState while the owning thread actually processed them.
        if GetKeyState(VK_LWIN.0 as i32).is_down() || GetKeyState(VK_RWIN.0 as i32).is_down() {
            mods.insert(keyboard_types::Modifiers::META);
        }
        // Toggle states live in the low bit of GetKeyState's return.
        if GetKeyState(VK_CAPITAL.0 as i32).is_toggled() {
            mods.insert(keyboard_types::Modifiers::CAPS_LOCK);
        }
        if GetKeyState(VK_NUMLOCK.0 as i32).is_toggled() {
            mods.insert(keyboard_types::Modifiers::NUM_LOCK);
        }
    }
    mods
}

trait KeyStateExt {
    fn is_down(&self) -> bool;
    fn is_toggled(&self) -> bool;
}

impl KeyStateExt for i16 {
    fn is_down(&self) -> bool {
        *self < 0
    }
    fn is_toggled(&self) -> bool {
        (self & 1) != 0
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => unsafe {
            let create_struct = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create_struct.lpCreateParams as isize);
            LRESULT(0)
        },

        WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP => {
            if let Some(event) = pointer_event_from_msg(hwnd, wparam) {
                let kind = match msg {
                    WM_POINTERDOWN => PointerEventKind::Down,
                    WM_POINTERUP => PointerEventKind::Up,
                    _ => PointerEventKind::Move,
                };
                with_extra(hwnd, |extra| {
                    extra.viewport.borrow_mut().pointer_event(hwnd, event, kind);
                    extra.viewport.borrow_mut().dispatch_and_redraw(hwnd);
                });
            }
            LRESULT(0)
        }

        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xffff) as i16 as f64 / 120.0;
            with_extra(hwnd, |extra| {
                let event = RosinPointerEvent { wheel_delta: Vec2::new(0.0, -delta), mods: current_modifiers(), ..RosinPointerEvent::default() };
                extra.viewport.borrow_mut().wheel_event(hwnd, event);
                extra.viewport.borrow_mut().dispatch_and_redraw(hwnd);
            });
            LRESULT(0)
        }

        WM_KEYDOWN => {
            // lparam bit 30 = previous key state; set on auto-repeat (matches
            // NSEvent.isARepeat on macOS).
            let repeat = (lparam.0 & 0x4000_0000) != 0;
            let vk = wparam.0 as u32;
            with_extra(hwnd, |extra| {
                let has_handler = extra.input_handler.borrow().is_some();
                let composing = extra
                    .input_handler
                    .borrow()
                    .as_deref()
                    .and_then(|h| h.composition_range())
                    .is_some();
                let consumed = ime::handle_editing_key(hwnd, vk, composing);
                // Printable keys deliver text through WM_CHAR, sibling to macOS insertText:,
                // so don't also surface a KeyboardEvent for them when a text field is focused.
                let text_key = has_handler && ime::is_text_key(vk);
                if !consumed && !text_key {
                    extra.viewport.borrow_mut().keyboard_event(hwnd, vk, true, repeat);
                }
                extra.viewport.borrow_mut().dispatch_and_redraw(hwnd);
            });
            LRESULT(0)
        }

        WM_KEYUP => {
            with_extra(hwnd, |extra| {
                extra.viewport.borrow_mut().keyboard_event(hwnd, wparam.0 as u32, false, false);
                extra.viewport.borrow_mut().dispatch_and_redraw(hwnd);
            });
            LRESULT(0)
        }

        WM_CHAR => {
            // One UTF-16 code unit per message; surrogates arrive as two consecutive ones.
            let code = wparam.0 as u16;
            with_extra(hwnd, |extra| {
                let ch = if (0xD800..0xDC00).contains(&code) {
                    extra.pending_high_surrogate.set(Some(code));
                    None
                } else if (0xDC00..0xE000).contains(&code) {
                    let ch = extra.pending_high_surrogate.get().and_then(|high| {
                        char::from_u32(0x10000 + (((high as u32) - 0xD800) << 10) + (code as u32 - 0xDC00))
                    });
                    extra.pending_high_surrogate.set(None);
                    ch
                } else {
                    extra.pending_high_surrogate.set(None);
                    char::from_u32(code as u32)
                };
                if let Some(c) = ch {
                    ime::insert_character(hwnd, c);
                }
            });
            LRESULT(0)
        }

        WM_UNICHAR => {
            // If queried (wParam == 0xFFFF), we answer "yes, we handle Unicode text".
            if wparam.0 == 0xFFFF {
                LRESULT(1)
            } else {
                if let Some(c) = char::from_u32(wparam.0 as u32) {
                    ime::insert_character(hwnd, c);
                }
                LRESULT(0)
            }
        }

        WM_GETDLGCODE => LRESULT((DLGC_WANTCHARS | DLGC_WANTARROWS | DLGC_WANTTAB) as isize),

        WM_IME_STARTCOMPOSITION => {
            ime::position_ime_windows(hwnd);
            LRESULT(0)
        }

        WM_IME_COMPOSITION => {
            ime::handle_ime_composition(hwnd, lparam.0 as u32);
            LRESULT(0)
        }

        WM_IME_ENDCOMPOSITION => {
            ime::handle_ime_end(hwnd);
            LRESULT(0)
        }

        WM_IME_SETCONTEXT => {
            // Suppress the OS-drawn composition window (our text widgets paint marked text
            // themselves); keep everything else in lParam, including the candidate window.
            let lparam = (lparam.0 as u32 & !IS_SHOWUICOMPOSITIONWINDOW) as isize;
            unsafe { DefWindowProcW(hwnd, msg, wparam, LPARAM(lparam)) }
        }

        WM_SIZE => {
            with_extra(hwnd, |extra| {
                let mut rect = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut rect);
                }
                let size = Size::new((rect.right - rect.left) as f64, (rect.bottom - rect.top) as f64);
                extra.viewport.borrow_mut().set_size(size);
                extra.needs_config.set(true);
                extra.viewport.borrow_mut().render(hwnd, extra);
            });
            LRESULT(0)
        }

        WM_PAINT => {
            with_extra(hwnd, |extra| extra.viewport.borrow_mut().render(hwnd, extra));
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            with_extra(hwnd, |extra| {
                let now = Instant::now();
                if let Some(last) = extra.last_frame.get() {
                    let dt = now.duration_since(last);
                    extra.viewport.borrow_mut().animation_frame(dt);
                }
                extra.last_frame.set(Some(now));
                let idle = extra.viewport.borrow().is_idle();
                if !idle {
                    extra.viewport.borrow_mut().render(hwnd, extra);
                }
            });
            LRESULT(0)
        }

        WM_ROSIN_CREATE_WINDOW => {
            // TODO: pop extra.pending_child_windows and call win::window::create_window for
            // each, mirroring ViewportTrait::create_window's downcast-and-dispatch in
            // mac/window.rs. Needs the same generic-erasure trick used there.
            LRESULT(0)
        }

        WM_SETCURSOR => {
            let hidden = with_extra(hwnd, |extra| extra.cursor_hidden.get()).unwrap_or(false);
            if hidden {
                unsafe { windows::Win32::UI::WindowsAndMessaging::SetCursor(None) };
                LRESULT(1)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }

        WM_DPICHANGED => {
            // TODO: read the new DPI from wparam's HIWORD and the suggested RECT from lparam,
            // resize/reposition via SetWindowPos, and update Viewport::set_scale -- mirrors
            // mac::window's use of backingScaleFactor in update_layer.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        WM_GETMINMAXINFO => {
            with_extra(hwnd, |extra| {
                unsafe {
                    let minmax = &mut *(lparam.0 as *mut windows::Win32::UI::WindowsAndMessaging::MINMAXINFO);
                    if let Some(min) = extra.min_size.get() {
                        minmax.ptMinTrackSize.x = min.width as i32;
                        minmax.ptMinTrackSize.y = min.height as i32;
                    }
                    if let Some(max) = extra.max_size.get() {
                        minmax.ptMaxTrackSize.x = max.width as i32;
                        minmax.ptMaxTrackSize.y = max.height as i32;
                    }
                }
            });
            LRESULT(0)
        }

        WM_ERASEBKGND => LRESULT(1), // wgpu/DXGI owns the client area; skip GDI's erase to avoid a flash.

        WM_CLOSE => {
            let stop_close = with_extra(hwnd, |extra| {
                // TODO: route through Viewport::queue_close_event + dispatch_event_queue,
                // same as mac::window::ViewportTrait::close, so an on_close handler can veto.
                let _ = extra;
                false
            })
            .unwrap_or(false);
            if !stop_close {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }

        WM_NCDESTROY => {
            // Last message a window ever receives -- reclaim the WindowExtra we boxed in
            // create_window so it doesn't leak.
            unsafe {
                let ptr = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowExtra;
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
