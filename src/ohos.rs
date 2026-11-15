#[cfg(feature = "audio")]
mod audio;
mod axis_scroll;
mod capture_pixels;
mod clipboard;
mod credentials;
mod cursor_visibility;
mod device;
mod dispatcher;
mod display;
mod file_access;
mod frame_delivery;
mod frame_demand;
mod image_alpha;
mod keyboard;
mod launch;
mod mouse_buttons;
#[cfg(feature = "native-vsync")]
mod native_frames;
mod path_targets;
mod platform;
mod platform_options;
#[cfg(feature = "native-input")]
mod pointer_position;
mod presentation;
mod quad_pipeline;
mod raw_file_assets;
mod render_cache;
mod screen_capture;
#[cfg(feature = "native-input")]
mod scroll_input;
mod scroll_packet;
mod seccomp_guard;
mod task_queue;
mod text_system;
mod touch_scroll;
mod viewport;
mod wgpu_atlas;
mod wgpu_context;
mod wgpu_renderer;
mod window;
mod window_manipulation;
mod window_state;
mod worker_state;
mod workers;

use std::{cell::RefCell, ffi::OsString, ops::Deref, sync::Arc};

use openharmony_ability::OpenHarmonyApp;
use parking_lot::Mutex;

pub use platform_options::PlatformOptions;

static REGISTERED_APP: Mutex<Option<(OpenHarmonyApp, PlatformOptions)>> = Mutex::new(None);

thread_local! {
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "The initializer is const; OHOS std TLS expansion triggers this lint."
    )]
    static EMBEDDED_APPLICATION: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
}

/// Retain an [`gpui::ApplicationHandle`] on the UI thread for the lifetime of the Ability.
///
/// On OpenHarmony, `OhosPlatform::run` registers an ArkTS UV callback and returns immediately,
/// so calling `gpui::Application::run` directly would drop `Rc<AppCell>` once `on_finish_launching`
/// completes. [`OhosApplication::run`] calls `run_embedded` and stores the handle here automatically.
pub fn retain_application_handle(handle: gpui::ApplicationHandle) {
    EMBEDDED_APPLICATION.with(|slot| {
        *slot.borrow_mut() = Some(handle);
    });
}

/// Wrapper around [`gpui::Application`] whose [`Self::run`] retains the underlying
/// [`gpui::ApplicationHandle`] across OpenHarmony's non-blocking event loop registration.
pub struct OhosApplication(gpui::Application);

impl OhosApplication {
    pub fn new(inner: gpui::Application) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> gpui::Application {
        self.0
    }

    pub fn with_assets(self, asset_source: impl gpui::AssetSource) -> Self {
        Self(self.0.with_assets(asset_source))
    }

    pub fn with_restart_arguments(self, arguments: Vec<OsString>) -> Self {
        Self(self.0.with_restart_arguments(arguments))
    }

    pub fn with_http_client(self, http_client: Arc<dyn gpui::http_client::HttpClient>) -> Self {
        Self(self.0.with_http_client(http_client))
    }

    pub fn with_quit_mode(self, mode: gpui::QuitMode) -> Self {
        Self(self.0.with_quit_mode(mode))
    }

    pub fn with_windowing(self, request: gpui::WindowingRequest) -> Self {
        Self(self.0.with_windowing(request))
    }

    pub fn on_open_urls<F>(&self, callback: F) -> &Self
    where
        F: 'static + FnMut(Vec<String>),
    {
        self.0.on_open_urls(callback);
        self
    }

    pub fn on_reopen<F>(&self, callback: F) -> &Self
    where
        F: 'static + FnMut(&mut gpui::App),
    {
        self.0.on_reopen(callback);
        self
    }

    pub fn run_embedded<F>(self, on_finish_launching: F) -> gpui::ApplicationHandle
    where
        F: 'static + FnOnce(&mut gpui::App),
    {
        self.0.run_embedded(on_finish_launching)
    }

    pub fn run<F>(self, on_finish_launching: F)
    where
        F: 'static + FnOnce(&mut gpui::App),
    {
        let handle = self.0.run_embedded(on_finish_launching);
        retain_application_handle(handle);
    }
}

impl Deref for OhosApplication {
    type Target = gpui::Application;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Register the process-wide [`OpenHarmonyApp`] and [`PlatformOptions`] before
/// invoking desktop-style zero-argument entry points such as [`application`].
pub fn set_openharmony_app(app: OpenHarmonyApp, options: PlatformOptions) {
    let _ = seccomp_guard::install_seccomp_sigsys_guard();
    *REGISTERED_APP.lock() = Some((app, options));
}

pub use seccomp_guard::{
    install_seccomp_sigsys_guard, is_seccomp_sigsys_guard_installed, unblock_sigsys_current_thread,
};

/// Return the previously registered [`OpenHarmonyApp`] and [`PlatformOptions`], if any.
pub fn registered_openharmony_app() -> Option<(OpenHarmonyApp, PlatformOptions)> {
    REGISTERED_APP.lock().clone()
}

pub fn try_current_platform(
    app: OpenHarmonyApp,
    options: PlatformOptions,
) -> anyhow::Result<std::rc::Rc<dyn gpui::Platform>> {
    Ok(std::rc::Rc::new(platform::OhosPlatform::new(app, options)?))
}

pub fn current_platform(app: OpenHarmonyApp, _headless: bool) -> std::rc::Rc<dyn gpui::Platform> {
    std::rc::Rc::new(
        platform::OhosPlatform::new(app, PlatformOptions::default())
            .inspect_err(|err| {
                log::error!("Failed to initialize OHOS platform: {}", err);
            })
            .unwrap_or_else(|_| panic!("Failed to initialize OHOS platform")),
    )
}

/// Build an OHOS [`gpui::Platform`] from the handle registered via [`set_openharmony_app`].
pub fn registered_current_platform(_headless: bool) -> std::rc::Rc<dyn gpui::Platform> {
    let (app, options) = registered_openharmony_app().unwrap_or_else(|| {
        panic!("OpenHarmonyApp has not been registered; call gpui_ohos::set_openharmony_app first")
    });
    try_current_platform(app, options)
        .inspect_err(|err| {
            log::error!("Failed to initialize OHOS platform: {}", err);
        })
        .unwrap_or_else(|_| panic!("Failed to initialize OHOS platform"))
}

/// Create an [`OhosApplication`] backed by the registered [`OpenHarmonyApp`],
/// matching desktop `gpui_platform::application()` so downstream apps need zero `main()` edits.
pub fn application() -> OhosApplication {
    OhosApplication::new(gpui::Application::with_platform(
        registered_current_platform(false),
    ))
}

/// Desktop-compatible facade matching `gpui_platform`'s zero-argument entry points.
pub mod desktop_platform {
    pub fn application() -> super::OhosApplication {
        super::application()
    }

    pub fn current_platform(headless: bool) -> std::rc::Rc<dyn gpui::Platform> {
        super::registered_current_platform(headless)
    }
}

pub use text_system::{FontCatalog, FontDesignCoordinate, FontFaceData, SyntheticFontStyle};
#[cfg(feature = "grid")]
pub use text_system::{GridLayoutError, GridLayoutRequest, GridTextCell};

/// Experimental font-catalog API; signatures may change between releases.
/// Platform and font services share face bytes, glyph IDs and immutable descriptors.
pub struct PlatformServices {
    pub platform: std::rc::Rc<dyn gpui::Platform>,
    pub fonts: FontCatalog,
}

impl PlatformServices {
    pub fn try_new(app: OpenHarmonyApp, options: PlatformOptions) -> anyhow::Result<Self> {
        let platform = platform::OhosPlatform::new(app, options)?;
        let fonts = platform.font_catalog();
        Ok(Self {
            platform: std::rc::Rc::new(platform),
            fonts,
        })
    }
}

pub use image_alpha::{ImagePolicy, ImageRegistrationError, PremultipliedImage, RasterImage};
pub use openharmony_ability_plugin_resource::ResourceBridgePlugin;
pub use raw_file_assets::RawFileAssets;

#[cfg(feature = "native-input")]
pub use pointer_position::NativePointerPosition;
#[cfg(feature = "native-input")]
pub use scroll_input::NativeScrollDelta;

#[cfg(feature = "audio")]
pub use audio::{
    AudioBridgePlugin, NativeAudioError, NativeAudioPlayer, NativeAudioPlayerOptions,
    NativeAudioService,
};
