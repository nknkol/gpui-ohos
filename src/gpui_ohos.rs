//! OpenHarmony platform support for GPUI.
//!
//! The platform backend lives outside the Zed workspace and is injected with
//! [`gpui::Application::with_platform`], matching the architecture used by
//! `gpui-mobile` for Android and iOS.

#[cfg(feature = "fast")]
extern crate gpui_fast as gpui;

mod ohos;

pub use gpui::*;
pub use ohos::{
    OhosApplication, PlatformOptions, application, current_platform,
    desktop_platform as platform, install_seccomp_sigsys_guard,
    is_seccomp_sigsys_guard_installed, registered_current_platform, registered_openharmony_app,
    retain_application_handle, set_openharmony_app, try_current_platform,
    unblock_sigsys_current_thread,
};

pub use ohos::{
    FontCatalog, FontDesignCoordinate, FontFaceData, PlatformServices, SyntheticFontStyle,
};
#[cfg(feature = "grid")]
pub use ohos::{GridLayoutError, GridLayoutRequest, GridTextCell};
pub use ohos::{ImagePolicy, ImageRegistrationError, PremultipliedImage, RasterImage};
pub use ohos::{RawFileAssets, ResourceBridgePlugin};

#[cfg(feature = "native-input")]
pub use ohos::{NativePointerPosition, NativeScrollDelta};

#[cfg(feature = "audio")]
pub use ohos::{
    AudioBridgePlugin, NativeAudioError, NativeAudioPlayer, NativeAudioPlayerOptions,
    NativeAudioService,
};
