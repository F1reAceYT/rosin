use std::{cell::RefCell, rc::Rc, sync::OnceLock, thread};
use std::panic;

use pollster::FutureExt;
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG, MB_ICONERROR, MB_OK,
};

use crate::{
    log::error,
    prelude::*,
    vello::{self, AaSupport},
    wgpu::{self, ExperimentalFeatures},
    win::window,
};

static APP_STARTED: OnceLock<()> = OnceLock::new();

pub(crate) struct AppLauncher<S: Sync + 'static> {
    windows: Vec<WindowDesc<S>>,
    _translation_map: Option<TranslationMap>,
    wgpu_config: WgpuConfig,
    _state: Option<Rc<RefCell<S>>>,

    #[cfg(all(feature = "hot-reload", debug_assertions))]
    hot_reloader: RefCell<Option<()>>, // TODO: crate::win::hot::HotReloader once it exists (see mac::hot for the shape to port).
}

impl<S: Sync + 'static> AppLauncher<S> {
    pub fn new(window: WindowDesc<S>) -> Self {
        Self {
            windows: vec![window],
            _translation_map: None,
            wgpu_config: WgpuConfig::default(),
            _state: None,

            #[cfg(all(feature = "hot-reload", debug_assertions))]
            hot_reloader: RefCell::new(None),
        }
    }

    pub fn with_wgpu_config(mut self, config: WgpuConfig) -> Self {
        self.wgpu_config = config;
        self
    }

    pub fn add_window(mut self, window: WindowDesc<S>) -> Self {
        self.windows.push(window);
        self
    }

    // No hot-reload, no serde requirement
    #[cfg(not(all(feature = "hot-reload", debug_assertions)))]
    pub fn run(self, state: S, translation_map: TranslationMap) -> Result<(), LaunchError> {
        let _ = translation_map; // TODO: thread through to Viewport::new in win::window::create_window, same as mac.
        self.run_impl(state)
    }

    // Yes hot-reload, yes serde requirement
    #[cfg(all(feature = "hot-reload", debug_assertions))]
    pub fn run(self, state: S, translation_map: TranslationMap) -> Result<(), LaunchError>
    where
        S: serde::Serialize + serde::de::DeserializeOwned + crate::typehash::TypeHash + 'static,
    {
        // TODO: port mac::app's ROSIN_HOT_RELOAD_SNAPSHOT bring-up once win::hot exists.
        let _ = translation_map;
        self.run_impl(state)
    }

    fn run_impl(self, state: S) -> Result<(), LaunchError> {
        if APP_STARTED.set(()).is_err() {
            return Err(LaunchError::AlreadyStarted);
        }

        // Start loading fonts in a background thread to reduce time to first frame, same as mac.
        let _ = thread::spawn(|| {
            if let Err(e) = panic::catch_unwind(global_font_ctx) {
                error!("Font loading thread panicked: {:?}", e);
            }
        });

        // --- wgpu/vello bring-up -------------------------------------------------------
        // Identical to mac::app::application_did_finish_launching_impl's GPU setup; there's
        // no macOS-specific API in any of this block, so it's a second strong candidate (along
        // with the render path noted in win/window.rs) for extraction into a shared
        // `gpu::init(wgpu_config) -> Result<(GpuCtx, VelloRenderer), LaunchError>` helper once
        // both backends are in the tree.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: self.wgpu_config.power_preference,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .block_on()
            .unwrap_or_else(|e| fatal_error_and_quit("GPU initialization failed", &format!("Failed to request a WGPU adapter.\n\n{e}")));

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("RosinDevice"),
                required_features: self.wgpu_config.features,
                required_limits: self.wgpu_config.limits.clone(),
                memory_hints: self.wgpu_config.memory_hints.clone(),
                trace: wgpu::Trace::Off,
                experimental_features: ExperimentalFeatures::disabled(),
            })
            .block_on()
            .unwrap_or_else(|e| fatal_error_and_quit("GPU initialization failed", &format!("Failed to create a WGPU device.\n\n{e}")));

        let compositor = Compositor {
            blitter: RefCell::new(None),
            custom: RefCell::new(None),
        };

        let vello_renderer = {
            let renderer = vello::Renderer::new(
                &device,
                vello::RendererOptions {
                    use_cpu: false,
                    antialiasing_support: AaSupport::all(),
                    num_init_threads: None,
                    pipeline_cache: None,
                },
            )
            .unwrap_or_else(|e| fatal_error_and_quit("Renderer initialization failed", &format!("Failed to create the Vello renderer.\n\n{e}")));

            Rc::new(RefCell::new(renderer))
        };

        let gpu_ctx = Rc::new(GpuCtx { instance, adapter, device, queue, compositor });
        let state = Rc::new(RefCell::new(state));

        // --- window creation -------------------------------------------------------------
        for desc in &self.windows {
            window::create_window(desc, state.clone(), gpu_ctx.clone(), vello_renderer.clone());
        }

        // --- message loop ------------------------------------------------------------------
        // A plain GetMessageW loop, same as any Win32 app. Because win::window's WM_TIMER
        // handler (1000/144 ms interval, see create_window) drives redraws directly rather than
        // relying on WM_PAINT's natural pacing, this loop doesn't need PeekMessage-based idle
        // rendering the way some game-style Win32 apps do.
        let mut msg = MSG::default();
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        Ok(())
    }
}

/// Windows equivalent of `mac::util::fatal_alert_and_quit`: show a message box, then abort.
/// Not recoverable by design -- a failed adapter/device request means the app can't render.
fn fatal_error_and_quit(title: &str, details: &str) -> ! {
    let mut title_w: Vec<u16> = title.encode_utf16().collect();
    title_w.push(0);
    let mut details_w: Vec<u16> = details.encode_utf16().collect();
    details_w.push(0);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::MessageBoxW(None, PCWSTR(details_w.as_ptr()), PCWSTR(title_w.as_ptr()), MB_OK | MB_ICONERROR);
    }
    std::process::exit(1);
}
