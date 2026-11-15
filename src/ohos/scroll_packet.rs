use gpui::{Point, ScrollDelta, px};

#[cfg(feature = "native-input")]
use super::scroll_input::{NativeScrollDelta, NativeScrollScope};

const WHEEL_DEGREES: f64 = 15.;

pub(super) struct ScrollPacket {
    pub(super) delta: ScrollDelta,
    #[cfg(feature = "native-input")]
    native: Option<NativeScrollDelta>,
}

impl ScrollPacket {
    pub(super) fn axis(raw: Point<f64>, scale: f32, wheel: bool, scroll_step: i32) -> Option<Self> {
        // OH_ArkUI_AxisEvent reports viewport scroll offset (positive = scroll down/right,
        // with system natural-scrolling preference already applied), whereas GPUI ScrollDelta
        // represents content displacement (negative = scroll down/right, matching Wayland -1.0).
        let raw = gpui::point(-raw.x, -raw.y);
        if wheel {
            Self::wheel(raw, scroll_step)
        } else {
            Self::pixels(raw, scale)
        }
    }

    /// Keep the system scroll-step coefficient in ordinary widget line deltas.
    pub(super) fn wheel(degrees: Point<f64>, _scroll_step: i32) -> Option<Self> {
        let lines = degrees.map(|value| value / WHEEL_DEGREES);
        let logical = lines.map(|value| value as f32);
        if !logical.x.is_finite() || !logical.y.is_finite() {
            return None;
        }
        Some(Self {
            delta: ScrollDelta::Lines(logical),
            #[cfg(feature = "native-input")]
            native: (_scroll_step > 0).then(|| {
                NativeScrollDelta::WheelTicks(lines.map(|value| value / f64::from(_scroll_step)))
            }),
        })
    }

    pub(super) fn pixels(device: Point<f64>, scale: f32) -> Option<Self> {
        if !scale.is_finite() || scale <= 0. {
            return None;
        }
        let logical = device.map(|value| px(value as f32 / scale));
        if !logical.x.as_f32().is_finite() || !logical.y.as_f32().is_finite() {
            return None;
        }
        Some(Self {
            delta: ScrollDelta::Pixels(logical),
            #[cfg(feature = "native-input")]
            native: Some(NativeScrollDelta::DevicePixels(device)),
        })
    }

    #[cfg(feature = "native-input")]
    pub(super) fn bind(&self, window: gpui::WindowId, scale: f32) -> NativeScrollScope {
        NativeScrollScope::bind(window, self.delta, scale, self.native)
    }
}
