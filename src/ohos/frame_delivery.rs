#[cfg(feature = "native-vsync")]
use std::time::{Duration, Instant};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use gpui::ForegroundExecutor;
use openharmony_ability::{FrameInputDelivery, OpenHarmonyApp};

#[cfg(feature = "native-vsync")]
use super::native_frames::NativeFrames;

#[cfg(feature = "native-vsync")]
const MAX_PANEL_HZ: u32 = 120;
use super::{
    frame_demand::{DeliveryChange, FrameDemand},
    window::OhosWindow,
};

pub(crate) struct FrameCallbackScope {
    active: Rc<Cell<bool>>,
    previous: bool,
}

impl FrameCallbackScope {
    pub(crate) fn enter(active: Rc<Cell<bool>>) -> Self {
        let previous = active.replace(true);
        Self { active, previous }
    }
}

impl Drop for FrameCallbackScope {
    fn drop(&mut self) {
        self.active.set(self.previous);
    }
}

pub(crate) struct WindowFrames {
    app: Rc<RefCell<Option<OpenHarmonyApp>>>,
    window_id: Cell<i64>,
    window: RefCell<Weak<RefCell<OhosWindow>>>,
    demand: RefCell<FrameDemand>,
    surface_available: Cell<bool>,
    foreground: ForegroundExecutor,
    sync_queued: Cell<bool>,
    callback_active: Rc<Cell<bool>>,
    register_warned: Cell<bool>,
    unregister_warned: Cell<bool>,
    #[cfg(feature = "native-vsync")]
    active: Cell<bool>,
    #[cfg(feature = "native-vsync")]
    native: RefCell<Option<NativeFrames>>,
    #[cfg(feature = "native-vsync")]
    native_cleanup_pending: Cell<bool>,
    #[cfg(feature = "native-vsync")]
    vsync_clock: Cell<Option<(i64, Instant)>>,
}

impl WindowFrames {
    pub(crate) fn new(
        window_id: i64,
        app: Rc<RefCell<Option<OpenHarmonyApp>>>,
        foreground: ForegroundExecutor,
        callback_active: Rc<Cell<bool>>,
    ) -> Rc<Self> {
        let frames = Rc::new(Self {
            app,
            window_id: Cell::new(window_id),
            window: RefCell::new(Weak::new()),
            demand: RefCell::new(FrameDemand::default()),
            surface_available: Cell::new(false),
            foreground,
            sync_queued: Cell::new(false),
            callback_active,
            register_warned: Cell::new(false),
            unregister_warned: Cell::new(false),
            #[cfg(feature = "native-vsync")]
            active: Cell::new(false),
            #[cfg(feature = "native-vsync")]
            native: RefCell::new(None),
            #[cfg(feature = "native-vsync")]
            native_cleanup_pending: Cell::new(false),
            #[cfg(feature = "native-vsync")]
            vsync_clock: Cell::new(None),
        });
        #[cfg(feature = "native-vsync")]
        super::native_frames::register(&frames);
        frames
    }

    pub(crate) fn set_window(&self, window: Weak<RefCell<OhosWindow>>) {
        *self.window.borrow_mut() = window;
    }

    pub(crate) fn rebind_window(self: &Rc<Self>, window_id: i64) {
        // The frame waker is cached by GPUI and must follow the logical window.
        self.window_id.set(window_id);
        #[cfg(feature = "native-vsync")]
        {
            self.native_cleanup_pending.set(true);
            self.active.set(false);
            super::native_frames::register(self);
        }
        self.demand.borrow_mut().surface_destroyed();
        self.surface_available.set(false);
        self.sync_frame_delivery();
    }

    pub(crate) fn request(self: &Rc<Self>) {
        self.demand.borrow_mut().request();
        self.sync_frame_delivery();
    }

    pub(crate) fn set_active(self: &Rc<Self>, visible: bool, surface_available: bool) {
        #[cfg(feature = "native-vsync")]
        self.active.set(visible && surface_available);
        if self.surface_available.replace(surface_available) && !surface_available {
            self.demand.borrow_mut().surface_destroyed();
        }
        self.demand
            .borrow_mut()
            .set_active(visible && surface_available);
        self.sync_frame_delivery();
    }

    fn sync_frame_delivery(self: &Rc<Self>) {
        let pending = self.demand.borrow().pending().is_some();
        #[cfg(feature = "native-vsync")]
        let pending = pending || self.native_cleanup_pending.get();
        if !pending {
            return;
        }
        if self.callback_active.get() {
            self.defer_sync();
            return;
        }
        #[cfg(feature = "native-vsync")]
        if self.native_cleanup_pending.replace(false) {
            self.native.borrow_mut().take();
        }
        let Some(change) = self.demand.borrow().pending() else {
            return;
        };
        let Some(app) = self.app.borrow().clone() else {
            return;
        };
        let register = change == DeliveryChange::Register;
        let result = if register {
            self.register_delivery(&app)
        } else {
            #[cfg(feature = "native-vsync")]
            {
                self.vsync_clock.set(None);
                if self.active.get() {
                    if let Some(native) = self.native.borrow_mut().as_mut() {
                        native.suspend();
                    }
                } else {
                    self.native.borrow_mut().take();
                }
                Ok(())
            }
            #[cfg(not(feature = "native-vsync"))]
            app.set_frame_input_delivery_for(self.window_id.get(), FrameInputDelivery::OnDemand)
        };
        self.demand.borrow_mut().complete(change, result.is_ok());
        if let Err(error) = result {
            let warned = if register {
                &self.register_warned
            } else {
                &self.unregister_warned
            };
            if !warned.replace(true) {
                let operation = if register { "register" } else { "unregister" };
                log::warn!(
                    "Cannot {operation} OHOS frame callback for window {}: {error}",
                    self.window_id.get()
                );
            }
        }
    }

    #[cfg(not(feature = "native-vsync"))]
    fn register_delivery(&self, app: &OpenHarmonyApp) -> napi_ohos::Result<()> {
        app.set_frame_input_delivery_for(self.window_id.get(), FrameInputDelivery::Continuous)
    }

    #[cfg(feature = "native-vsync")]
    fn register_delivery(&self, app: &OpenHarmonyApp) -> napi_ohos::Result<()> {
        // `refresh_rate()` is the panel's CURRENT rate: after an idle LTPO drop it reads 60,
        // and voting max=60 would pin the panel at 60Hz (self-reinforcing lock). Always vote
        // for the panel's top rate instead.
        let hz = app.refresh_rate().max(MAX_PANEL_HZ);
        if let Some(native) = self.native.borrow_mut().as_mut() {
            return native.resume(hz);
        }
        app.set_frame_input_delivery_for(self.window_id.get(), FrameInputDelivery::OnDemand)?;
        let native = NativeFrames::new(self.window_id.get(), hz, app.main_thread()?)?;
        *self.native.borrow_mut() = Some(native);
        log::info!(
            "native-frame-source window={} target_hz={hz}",
            self.window_id.get()
        );
        Ok(())
    }

    fn defer_sync(self: &Rc<Self>) {
        if self.sync_queued.replace(true) {
            return;
        }
        let frames = self.clone();
        // Keep cleanup alive through window drop; redraw events never drain this queue.
        self.foreground
            .spawn(async move {
                frames.sync_queued.set(false);
                frames.sync_frame_delivery();
            })
            .detach();
    }

    fn on_callback(self: &Rc<Self>) {
        if self.demand.borrow_mut().on_callback().is_some() {
            self.defer_sync();
        }
    }

    #[cfg(not(feature = "native-vsync"))]
    pub(crate) fn on_xcomponent_callback(self: &Rc<Self>) {
        self.on_callback();
    }

    #[cfg(feature = "native-vsync")]
    pub(crate) fn window_id(&self) -> i64 {
        self.window_id.get()
    }

    #[cfg(feature = "native-vsync")]
    fn resolve_vsync_instant(&self, vsync_timestamp_ns: i64) -> Instant {
        let wall_now = Instant::now();
        if vsync_timestamp_ns <= 0 {
            self.vsync_clock.set(None);
            return wall_now;
        }
        if let Some((base_ns, base_instant)) = self.vsync_clock.get() {
            let delta_ns = vsync_timestamp_ns - base_ns;
            // Keep locked to hardware VSync cadence across active bursts; re-anchor only
            // after an idle gap (> 250ms) or clock discontinuity.
            if (0..=250_000_000).contains(&delta_ns) {
                let next_instant = base_instant + Duration::from_nanos(delta_ns as u64);
                self.vsync_clock
                    .set(Some((vsync_timestamp_ns, next_instant)));
                return next_instant;
            }
        }
        self.vsync_clock.set(Some((vsync_timestamp_ns, wall_now)));
        wall_now
    }

    #[cfg(feature = "native-vsync")]
    pub(crate) fn on_native_callback(self: &Rc<Self>, token: usize, vsync_timestamp_ns: i64) {
        if self
            .native
            .borrow()
            .as_ref()
            .is_none_or(|native| native.is_paused() || native.token() != token)
        {
            return;
        }
        let frame_time = self.resolve_vsync_instant(vsync_timestamp_ns);
        let cb_start = Instant::now();
        let _scope = FrameCallbackScope::enter(self.callback_active.clone());
        self.on_callback();
        // Arm before drawing, including the idle-threshold draw, which may
        // create fresh demand. Deferred cleanup suspends or releases an idle receiver.
        if self.active.get() && self.demand.borrow().pending() != Some(DeliveryChange::Unregister) {
            let result = self.native.borrow_mut().as_mut().map(NativeFrames::request);
            if let Some(Err(error)) = result {
                log::warn!(
                    "NativeVSync stopped for window {}: {error}",
                    self.window_id.get()
                );
                self.demand
                    .borrow_mut()
                    .complete(DeliveryChange::Register, false);
                self.native_cleanup_pending.set(true);
                self.defer_sync();
            }
        }
        if let Some(window) = self.window.borrow().upgrade() {
            window.borrow().draw_native_frame(frame_time);
        }
        vsync_stats::note(vsync_timestamp_ns, cb_start, cb_start.elapsed());
    }
}

/// Per-second VSync pacing diagnostics (hilog `ndsh-vsync`): hardware VSync gaps
/// (skipped panel slots), main-thread callback arrival gaps and callback (draw) cost.
#[cfg(feature = "native-vsync")]
mod vsync_stats {
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct S {
        since: Option<Instant>,
        last_ts: i64,
        last_arrival: Option<Instant>,
        n: u32,
        hw_gap_sum: i64,
        hw_gap_max: i64,
        skipped: u32,
        arr_gap_max: Duration,
        cb_sum: Duration,
        cb_max: Duration,
        cb_over: u32,
    }
    thread_local! { static ST: RefCell<S> = RefCell::new(S::default()); }

    pub(super) fn note(ts: i64, arrival: Instant, cb: Duration) {
        ST.with_borrow_mut(|s| {
            let since = *s.since.get_or_insert(arrival);
            if s.last_ts > 0 && ts > s.last_ts && ts - s.last_ts < 500_000_000 {
                let gap = ts - s.last_ts;
                s.hw_gap_sum += gap;
                s.hw_gap_max = s.hw_gap_max.max(gap);
                if gap > 12_500_000 {
                    s.skipped += ((gap + 4_166_666) / 8_333_333 - 1) as u32;
                }
            }
            if let Some(prev) = s.last_arrival {
                let g = arrival.saturating_duration_since(prev);
                if g < Duration::from_millis(500) {
                    s.arr_gap_max = s.arr_gap_max.max(g);
                }
            }
            s.last_ts = ts;
            s.last_arrival = Some(arrival);
            s.n += 1;
            s.cb_sum += cb;
            s.cb_max = s.cb_max.max(cb);
            if cb > Duration::from_micros(8_333) {
                s.cb_over += 1;
            }
            let el = arrival.saturating_duration_since(since);
            if el >= Duration::from_secs(5) {
                let n = s.n.max(1);
                log::info!(
                    "ndsh-vsync: cb={} ({:.0}/s) hw_gap_avg={:.2}ms hw_gap_max={:.2}ms skipped_slots={} arrival_gap_max={:.2}ms cb_avg={:.2}ms cb_max={:.2}ms cb_over_budget={}",
                    s.n,
                    s.n as f64 / el.as_secs_f64(),
                    s.hw_gap_sum as f64 / (n.saturating_sub(1).max(1)) as f64 / 1e6,
                    s.hw_gap_max as f64 / 1e6,
                    s.skipped,
                    s.arr_gap_max.as_secs_f64() * 1e3,
                    s.cb_sum.as_secs_f64() * 1e3 / n as f64,
                    s.cb_max.as_secs_f64() * 1e3,
                    s.cb_over
                );
                *s = S { last_ts: s.last_ts, last_arrival: s.last_arrival, ..S::default() };
            }
        });
    }
}
