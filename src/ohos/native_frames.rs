use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::atomic::{AtomicUsize, Ordering},
};

use napi_ohos::{Error, Result};
use ohos_vsync_binding::{OH_NativeVSync_ExpectedRateRange as ExpectedRateRange, Vsync};
use openharmony_ability::MainThreadScheduler;

use super::frame_delivery::WindowFrames;

thread_local! {
    #[allow(clippy::missing_const_for_thread_local, reason = "The initializer is const; OHOS std TLS expansion triggers this lint.")]
    static WINDOWS: RefCell<Vec<Weak<WindowFrames>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn register(frames: &Rc<WindowFrames>) {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|window| {
            window
                .upgrade()
                .is_some_and(|window| window.window_id() != frames.window_id())
        });
        windows.push(Rc::downgrade(frames));
    });
}

/// Pin the vote to exactly `hz` (the panel's TOP rate). A loose range lets the LTPO
/// arbiter settle on an intermediate rate (e.g. 90Hz -> ~11ms intervals, 70-90 fps).
fn vote(hz: i32) -> ExpectedRateRange {
    ExpectedRateRange {
        min: hz,
        max: hz,
        expected: hz,
    }
}

static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);

/// The binding owns the receiver and its callback liveness gate. A generation
/// token also rejects UI-thread callbacks queued before this owner was dropped.
pub(crate) struct NativeFrames {
    vsync: Vsync,
    token: usize,
    hz: i32,
    paused: bool,
    main_thread: MainThreadScheduler,
    window_id: i64,
}

impl NativeFrames {
    pub(crate) fn new(window_id: i64, hz: u32, main_thread: MainThreadScheduler) -> Result<Self> {
        let hz = i32::try_from(hz)
            .ok()
            .filter(|hz| *hz > 0)
            .ok_or_else(|| Error::from_reason(format!("Invalid display refresh rate: {hz}")))?;
        let vsync = Vsync::try_new("gpui-native-render")
            .ok_or_else(|| Error::from_reason("NativeVSync creation failed"))?;
        // NOTE: Do NOT enable `OH_NativeVSync_SetDVSyncSwitch(true)` on GLES (`eglSwapBuffers`)
        // without `OH_NativeWindow` `SET_UI_TIMESTAMP` ahead-of-phase buffer tagging; otherwise
        // Rosen compositor buffer-queue phase slips cause intermittent 2-way scroll ghosting.
        let result = vsync.set_expected_frame_rate_range(vote(hz));
        if result != 0 {
            return Err(Error::from_reason(format!(
                "NativeVSync frame-rate request failed: {result}"
            )));
        }
        let mut connection = Self {
            vsync,
            token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
            hz,
            paused: false,
            main_thread,
            window_id,
        };
        connection.request()?;
        Ok(connection)
    }

    pub(crate) fn token(&self) -> usize {
        self.token
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused
    }

    /// Suspend armed callbacks while idle without destroying the underlying
    /// `OH_NativeVSync` receiver, eliminating first-input wakeup IPC/allocation spikes.
    pub(crate) fn suspend(&mut self) {
        self.paused = true;
        self.token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    }

    /// Resume a warm `OH_NativeVSync` receiver, updating rate only if the panel Hz changed.
    pub(crate) fn resume(&mut self, hz: u32) -> Result<()> {
        let hz = i32::try_from(hz)
            .ok()
            .filter(|hz| *hz > 0)
            .ok_or_else(|| Error::from_reason(format!("Invalid display refresh rate: {hz}")))?;
        // Re-vote on every resume: an idle receiver's vote may have been dropped by the
        // frame-rate arbiter, and a stale lower vote must never survive.
        let result = self.vsync.set_expected_frame_rate_range(vote(hz));
        if result != 0 {
            return Err(Error::from_reason(format!(
                "NativeVSync frame-rate request failed: {result}"
            )));
        }
        self.hz = hz;
        self.paused = false;
        self.token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        self.request()
    }

    pub(crate) fn request(&mut self) -> Result<()> {
        let main_thread = self.main_thread.clone();
        let window_id = self.window_id;
        let token = self.token;
        let result = self.vsync.request_frame_once(move |vsync_timestamp_ns| {
            // Binding callbacks may run on the VSync thread; GPUI stays on its UI thread.
            // Forward the exact hardware VSync nanosecond timestamp so scroll physics never
            // suffers from cross-thread wakeup jitter.
            if let Err(error) = main_thread.dispatch(move || {
                let frames = WINDOWS.with_borrow(|windows| {
                    windows
                        .iter()
                        .filter_map(Weak::upgrade)
                        .find(|frames| frames.window_id() == window_id)
                });
                if let Some(frames) = frames {
                    frames.on_native_callback(token, vsync_timestamp_ns);
                }
            }) {
                log::warn!("Cannot dispatch NativeVSync for window {window_id}: {error}");
            }
        });
        if result == 0 {
            Ok(())
        } else {
            Err(Error::from_reason(format!(
                "NativeVSync frame request failed: {result}"
            )))
        }
    }
}

impl Drop for NativeFrames {
    fn drop(&mut self) {
        // Withdraw the vote before the binding destroys the receiver. Its
        // liveness gate suppresses late native callbacks without raw userdata.
        self.vsync.set_expected_frame_rate_range(ExpectedRateRange {
            min: 0,
            max: 0,
            expected: 0,
        });
    }
}
