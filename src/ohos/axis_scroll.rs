//! Touchpad coasting momentum and 120Hz stepless mouse-wheel physics for ArkUI axis events.
//!
//! On OpenHarmony desktop/2in1 devices, `OH_NativeXComponent`'s UI axis callback
//! (`OH_ArkUI_AxisEvent`) only delivers touchpad scroll deltas while fingers remain in
//! contact (`BEGIN` -> `UPDATE` -> `END`), without post-release coasting packets, and
//! delivers discrete mouse-wheel detents as instantaneous line jumps.
//!
//! [`AxisScroll`] provides window-owned 120Hz frame-driven stepless physics for both:
//! - **Touchpad (`ScrollDelta::Pixels`)**: 1:1 finger tracking during contact, recency-weighted
//!   velocity estimation over hardware timestamps with pause-before-lift detection, and
//!   smooth friction coasting after release (preserving `TouchPhase::Started` -> `TouchPhase::Moved`
//!   -> `TouchPhase::Ended` for overscroll bounce).
//! - **Mouse wheel (`ScrollDelta::Pixels`)**: critically-damped 120Hz continuous sub-pixel
//!   spring-damper integrator (`zeta = 1.0`) with zero first-frame jump, zero double-step on input
//!   frames, and smooth rapid-spin momentum accumulation. Discrete `ScrollDelta::Lines` are
//!   preserved when zoom/command modifiers (`Ctrl`/`Super`/`Alt`) are held.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use gpui::{
    DispatchEventResult, Modifiers, Pixels, PlatformInput, Point, ScrollDelta, ScrollEvent,
    ScrollSource, TouchPhase, point, px,
};

const MAX_FLING_SPEED: f32 = 8500.0;
const MIN_TOUCHPAD_FLING_SPEED: f32 = 50.0;
const STOP_SPEED: f32 = 4.5;
// Silky HarmonyOS PC / trackpad coasting curve: exp(-2.25 * t).
const TOUCHPAD_DECELERATION: f32 = 2.25;
const MIN_GESTURE_TRAVEL_PX: f32 = 3.0;

// Hardware event timestamps are nanoseconds.
const VELOCITY_WINDOW_NS: i64 = 75_000_000;
const PAUSE_BEFORE_LIFT_NS: i64 = 58_000_000;
const MIN_VELOCITY_DT_SEC: f32 = 0.008;
const MAX_VELOCITY_DT_SEC: f32 = 0.140;
const DOMINANT_AXIS_RATIO: f32 = 5.0;

// Critically-damped stepless mouse wheel physics (locked to hardware VSync + device-pixel error diffusion).
// Tuned for natural HarmonyOS PC desktop reading (~48px per 3-line detent, ~235ms silky glide tail).
pub(crate) const WHEEL_PIXELS_PER_LINE: f32 = 16.0;
const WHEEL_OMEGA: f32 = 16.5;
const WHEEL_INITIAL_VELOCITY_GAIN: f32 = 0.38;
const WHEEL_STOP_EPSILON_PX: f32 = 0.08;
const WHEEL_STOP_SPEED_PX: f32 = 0.8;
const WHEEL_RAPID_SPIN_WINDOW: Duration = Duration::from_millis(125);
const WHEEL_MAX_SPIN_BOOST: f32 = 1.65;
const WHEEL_MAX_REMAINING_PX: f32 = 2200.0;
// VSync-locked touchpad contact follower (~68% per 8.33ms frame, <2.5ms phase lag)
// eliminating ~125Hz touchpad vs 120Hz display `[2x, 0x]` beat strobing ("残影").
const TOUCHPAD_CONTACT_OMEGA: f32 = 135.0;
const TOUCHPAD_CONTACT_FLUSH_IDLE: Duration = Duration::from_millis(24);

#[derive(Clone, Copy)]
struct VelocitySample {
    timestamp_ns: i64,
    delta: Point<f32>,
}

struct TouchpadGesture {
    position: Point<Pixels>,
    modifiers: Modifiers,
    started_ts_ns: i64,
    last_move_ts_ns: i64,
    samples: VecDeque<VelocitySample>,
    total_distance: f32,
    pending_logical: Point<f32>,
    carry_dev: Point<f32>,
    last_input_at: Instant,
    last_tick_at: Option<Instant>,
}

struct TouchpadMomentum {
    position: Point<Pixels>,
    modifiers: Modifiers,
    direction: Point<f32>,
    speed: f32,
    started_at: Instant,
    duration: Duration,
    emitted_distance: f32,
    carry_dev: Point<f32>,
}

struct WheelInertia {
    position: Point<Pixels>,
    modifiers: Modifiers,
    remaining_px: Point<f32>,
    velocity_px: Point<f32>,
    carry_dev: Point<f32>,
    last_step_at: Instant,
    last_notch_at: Instant,
    spin_boost: f32,
    started_emitted: bool,
}

#[derive(Default)]
pub(crate) struct AxisScroll {
    epoch: Option<Instant>,
    touchpad_gesture: Option<TouchpadGesture>,
    touchpad_momentum: Option<TouchpadMomentum>,
    wheel_inertia: Option<WheelInertia>,
}

impl AxisScroll {
    pub(crate) fn has_momentum(&self) -> bool {
        self.touchpad_momentum.is_some()
            || self.wheel_inertia.is_some()
            || self
                .touchpad_gesture
                .as_ref()
                .is_some_and(|g| g.pending_logical.x != 0.0 || g.pending_logical.y != 0.0)
    }

    fn normalize_timestamp_ns(&mut self, raw_timestamp_ns: i64, now: Instant) -> i64 {
        if raw_timestamp_ns > 0 {
            return raw_timestamp_ns;
        }
        let epoch = *self.epoch.get_or_insert(now);
        now.saturating_duration_since(epoch)
            .as_nanos()
            .min(i64::MAX as u128) as i64
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_touchpad(
        &mut self,
        position: Point<Pixels>,
        delta: Point<Pixels>,
        phase: TouchPhase,
        modifiers: Modifiers,
        raw_timestamp_ns: i64,
        scale: f32,
        now: Instant,
        mut emit: impl FnMut(PlatformInput) -> DispatchEventResult,
    ) {
        if let Some(wheel) = self.wheel_inertia.take() {
            if wheel.started_emitted {
                emit(scroll_pixels(
                    wheel.position,
                    Point::default(),
                    wheel.modifiers,
                    TouchPhase::Ended,
                ));
            }
        }
        let ts_ns = self.normalize_timestamp_ns(raw_timestamp_ns, now);
        let delta_f32 = point(delta.x.as_f32(), delta.y.as_f32());
        let nonzero = delta_f32.x != 0.0 || delta_f32.y != 0.0;

        match phase {
            TouchPhase::Started => {
                self.touchpad_momentum = None;
                let mut gesture = TouchpadGesture {
                    position,
                    modifiers,
                    started_ts_ns: ts_ns,
                    last_move_ts_ns: ts_ns,
                    samples: VecDeque::with_capacity(16),
                    total_distance: 0.0,
                    pending_logical: Point::default(),
                    carry_dev: Point::default(),
                    last_input_at: now,
                    last_tick_at: None,
                };
                if nonzero {
                    Self::record_sample(&mut gesture, ts_ns, delta_f32);
                    gesture.pending_logical = delta_f32;
                }
                self.touchpad_gesture = Some(gesture);
                emit(scroll_pixels(
                    position,
                    Point::default(),
                    modifiers,
                    TouchPhase::Started,
                ));
            }
            TouchPhase::Moved => {
                if self.touchpad_momentum.take().is_some() {
                    emit(scroll_pixels(
                        position,
                        Point::default(),
                        modifiers,
                        TouchPhase::Ended,
                    ));
                }
                let is_new_gesture = self.touchpad_gesture.is_none();
                let gesture = self.touchpad_gesture.get_or_insert_with(|| TouchpadGesture {
                    position,
                    modifiers,
                    started_ts_ns: ts_ns,
                    last_move_ts_ns: ts_ns,
                    samples: VecDeque::with_capacity(16),
                    total_distance: 0.0,
                    pending_logical: Point::default(),
                    carry_dev: Point::default(),
                    last_input_at: now,
                    last_tick_at: None,
                });
                gesture.position = position;
                gesture.modifiers = modifiers;
                if is_new_gesture {
                    emit(scroll_pixels(
                        position,
                        Point::default(),
                        modifiers,
                        TouchPhase::Started,
                    ));
                }
                if nonzero {
                    Self::record_sample(gesture, ts_ns, delta_f32);
                    gesture.last_input_at = now;
                    // Coalesce contact deltas for VSync-locked emission in `tick()` so ~125Hz
                    // touchpad packets never beat against 120Hz display VSync (eliminating 2x/0x judder).
                    gesture.pending_logical.x += delta_f32.x;
                    gesture.pending_logical.y += delta_f32.y;
                }
            }
            TouchPhase::Ended => {
                if nonzero {
                    if let Some(gesture) = self.touchpad_gesture.as_mut() {
                        Self::record_sample(gesture, ts_ns, delta_f32);
                        gesture.pending_logical.x += delta_f32.x;
                        gesture.pending_logical.y += delta_f32.y;
                    }
                }
                let mut carry_dev = Point::default();
                let mut pending_logical = Point::default();
                let velocity = self.touchpad_gesture.take().and_then(|gesture| {
                    carry_dev = gesture.carry_dev;
                    pending_logical = gesture.pending_logical;
                    Self::estimate_velocity(&gesture, ts_ns)
                });
                if let Some((direction, speed)) = velocity {
                    if pending_logical.x != 0.0 || pending_logical.y != 0.0 {
                        let snapped = quantize_device_step(pending_logical, &mut carry_dev, scale);
                        if snapped.x != 0.0 || snapped.y != 0.0 {
                            emit(scroll_pixels(
                                position,
                                point(px(snapped.x), px(snapped.y)),
                                modifiers,
                                TouchPhase::Moved,
                            ));
                        }
                    }
                    let capped_speed = speed.min(MAX_FLING_SPEED);
                    let duration = Duration::from_secs_f32(
                        (capped_speed / STOP_SPEED).ln() / TOUCHPAD_DECELERATION,
                    );
                    self.touchpad_momentum = Some(TouchpadMomentum {
                        position,
                        modifiers,
                        direction,
                        speed: capped_speed,
                        started_at: now,
                        duration,
                        emitted_distance: 0.0,
                        carry_dev,
                    });
                } else {
                    let flushed = flush_device_step(pending_logical, &mut carry_dev, scale);
                    emit(scroll_pixels(
                        position,
                        point(px(flushed.x), px(flushed.y)),
                        modifiers,
                        TouchPhase::Ended,
                    ));
                }
            }
            TouchPhase::Cancelled => {
                self.touchpad_gesture = None;
                self.touchpad_momentum = None;
                emit(scroll_pixels(
                    position,
                    Point::default(),
                    modifiers,
                    TouchPhase::Cancelled,
                ));
            }
        }
    }

    pub(crate) fn handle_wheel(
        &mut self,
        position: Point<Pixels>,
        lines: Point<f32>,
        phase: TouchPhase,
        modifiers: Modifiers,
        now: Instant,
        mut emit: impl FnMut(PlatformInput) -> DispatchEventResult,
    ) {
        if let Some(momentum) = self.touchpad_momentum.take() {
            emit(scroll_pixels(
                momentum.position,
                Point::default(),
                momentum.modifiers,
                TouchPhase::Ended,
            ));
        }

        // Preserve discrete line ticks when modifier keys are held (e.g. Ctrl+Wheel zoom).
        if modifiers.control || modifiers.platform || modifiers.alt {
            if let Some(wheel) = self.wheel_inertia.take() {
                if wheel.started_emitted {
                    emit(scroll_pixels(
                        wheel.position,
                        Point::default(),
                        wheel.modifiers,
                        TouchPhase::Ended,
                    ));
                }
            }
            if lines.x != 0.0 || lines.y != 0.0 {
                emit(scroll_lines(position, lines, modifiers, phase));
            }
            return;
        }

        if lines.x == 0.0 && lines.y == 0.0 {
            return;
        }

        let add_px = point(
            lines.x * WHEEL_PIXELS_PER_LINE,
            lines.y * WHEEL_PIXELS_PER_LINE,
        );

        match self.wheel_inertia.as_mut() {
            Some(existing) => {
                if add_px.x != 0.0
                    && existing.remaining_px.x != 0.0
                    && add_px.x.signum() != existing.remaining_px.x.signum()
                {
                    existing.remaining_px.x = 0.0;
                    existing.velocity_px.x = 0.0;
                    existing.carry_dev.x = 0.0;
                    existing.spin_boost = 1.0;
                }
                if add_px.y != 0.0
                    && existing.remaining_px.y != 0.0
                    && add_px.y.signum() != existing.remaining_px.y.signum()
                {
                    existing.remaining_px.y = 0.0;
                    existing.velocity_px.y = 0.0;
                    existing.carry_dev.y = 0.0;
                    existing.spin_boost = 1.0;
                }
                let since_last = now.saturating_duration_since(existing.last_notch_at);
                if since_last <= WHEEL_RAPID_SPIN_WINDOW {
                    existing.spin_boost = (existing.spin_boost + 0.12).min(WHEEL_MAX_SPIN_BOOST);
                } else {
                    existing.spin_boost = 1.0;
                }
                let boosted = point(add_px.x * existing.spin_boost, add_px.y * existing.spin_boost);
                existing.remaining_px.x = (existing.remaining_px.x + boosted.x)
                    .clamp(-WHEEL_MAX_REMAINING_PX, WHEEL_MAX_REMAINING_PX);
                existing.remaining_px.y = (existing.remaining_px.y + boosted.y)
                    .clamp(-WHEEL_MAX_REMAINING_PX, WHEEL_MAX_REMAINING_PX);
                // Blend forward velocity impulse smoothly so consecutive detents glide as one continuous stream.
                existing.velocity_px.x += boosted.x * (WHEEL_OMEGA * 0.24);
                existing.velocity_px.y += boosted.y * (WHEEL_OMEGA * 0.24);
                existing.position = position;
                existing.modifiers = modifiers;
                existing.last_notch_at = now;
            }
            None => {
                emit(scroll_pixels(
                    position,
                    Point::default(),
                    modifiers,
                    TouchPhase::Started,
                ));
                let rem = point(
                    add_px
                        .x
                        .clamp(-WHEEL_MAX_REMAINING_PX, WHEEL_MAX_REMAINING_PX),
                    add_px
                        .y
                        .clamp(-WHEEL_MAX_REMAINING_PX, WHEEL_MAX_REMAINING_PX),
                );
                self.wheel_inertia = Some(WheelInertia {
                    position,
                    modifiers,
                    remaining_px: rem,
                    velocity_px: point(
                        rem.x * (WHEEL_OMEGA * WHEEL_INITIAL_VELOCITY_GAIN),
                        rem.y * (WHEEL_OMEGA * WHEEL_INITIAL_VELOCITY_GAIN),
                    ),
                    carry_dev: Point::default(),
                    // Seed ~6ms lead so the very first VSync advances immediately without startup lag.
                    last_step_at: now
                        .checked_sub(Duration::from_millis(6))
                        .unwrap_or(now),
                    last_notch_at: now,
                    spin_boost: 1.0,
                    started_emitted: true,
                });
            }
        }
    }

    pub(crate) fn tick(&mut self, now: Instant, scale: f32) -> Option<PlatformInput> {
        if let Some(gesture) = self.touchpad_gesture.as_mut() {
            let dt = gesture
                .last_tick_at
                .map(|prev| now.saturating_duration_since(prev).as_secs_f32())
                .unwrap_or(1.0 / 120.0)
                .clamp(1.0 / 240.0, 0.032);
            gesture.last_tick_at = Some(now);
            if gesture.pending_logical.x != 0.0 || gesture.pending_logical.y != 0.0 {
                let s = if scale.is_finite() && scale > 0.0 {
                    scale
                } else {
                    1.0
                };
                let idle_since_input = now.saturating_duration_since(gesture.last_input_at);
                let dist_dev =
                    (gesture.pending_logical.x * s).hypot(gesture.pending_logical.y * s);
                let step = if idle_since_input >= TOUCHPAD_CONTACT_FLUSH_IDLE || dist_dev <= 0.45 {
                    std::mem::take(&mut gesture.pending_logical)
                } else {
                    let alpha = (1.0 - (-TOUCHPAD_CONTACT_OMEGA * dt).exp()).clamp(0.45, 0.92);
                    let sx = gesture.pending_logical.x * alpha;
                    let sy = gesture.pending_logical.y * alpha;
                    gesture.pending_logical.x -= sx;
                    gesture.pending_logical.y -= sy;
                    point(sx, sy)
                };
                let snapped = quantize_device_step(step, &mut gesture.carry_dev, scale);
                if snapped.x != 0.0 || snapped.y != 0.0 {
                    return Some(scroll_pixels(
                        gesture.position,
                        point(px(snapped.x), px(snapped.y)),
                        gesture.modifiers,
                        TouchPhase::Moved,
                    ));
                }
            }
        }

        if let Some(momentum) = self.touchpad_momentum.as_mut() {
            let elapsed = now
                .saturating_duration_since(momentum.started_at)
                .min(momentum.duration);
            let distance = momentum.speed / TOUCHPAD_DECELERATION
                * (1.0 - (-TOUCHPAD_DECELERATION * elapsed.as_secs_f32()).exp());
            let step = (distance - momentum.emitted_distance).max(0.0);
            momentum.emitted_distance = distance.max(momentum.emitted_distance);
            let ended = elapsed >= momentum.duration;
            let raw_step = point(momentum.direction.x * step, momentum.direction.y * step);
            if ended {
                let flushed = flush_device_step(raw_step, &mut momentum.carry_dev, scale);
                let input = scroll_pixels(
                    momentum.position,
                    point(px(flushed.x), px(flushed.y)),
                    momentum.modifiers,
                    TouchPhase::Ended,
                );
                self.touchpad_momentum = None;
                return Some(input);
            }
            let snapped = quantize_device_step(raw_step, &mut momentum.carry_dev, scale);
            if snapped.x == 0.0 && snapped.y == 0.0 {
                return None;
            }
            return Some(scroll_pixels(
                momentum.position,
                point(px(snapped.x), px(snapped.y)),
                momentum.modifiers,
                TouchPhase::Moved,
            ));
        }

        let wheel = self.wheel_inertia.as_mut()?;
        let dt = now
            .saturating_duration_since(wheel.last_step_at)
            .as_secs_f32()
            .clamp(1.0 / 240.0, 0.032);
        wheel.last_step_at = now;

        // Analytical critically-damped spring-damper step (`x'' + 2*w*x' + w^2*x = 0`)
        // driving `remaining_px -> 0` smoothly with zero overshoot and C1 continuous velocity.
        let (rem_x, vel_x, step_x) =
            step_critically_damped(wheel.remaining_px.x, wheel.velocity_px.x, WHEEL_OMEGA, dt);
        let (rem_y, vel_y, step_y) =
            step_critically_damped(wheel.remaining_px.y, wheel.velocity_px.y, WHEEL_OMEGA, dt);
        wheel.remaining_px = point(rem_x, rem_y);
        wheel.velocity_px = point(vel_x, vel_y);

        let dist_left = wheel.remaining_px.x.hypot(wheel.remaining_px.y);
        let speed_left = wheel.velocity_px.x.hypot(wheel.velocity_px.y);

        if dist_left <= WHEEL_STOP_EPSILON_PX && speed_left <= WHEEL_STOP_SPEED_PX {
            let final_logical = point(
                step_x + wheel.remaining_px.x,
                step_y + wheel.remaining_px.y,
            );
            let flushed = flush_device_step(final_logical, &mut wheel.carry_dev, scale);
            let input = scroll_pixels(
                wheel.position,
                point(px(flushed.x), px(flushed.y)),
                wheel.modifiers,
                TouchPhase::Ended,
            );
            self.wheel_inertia = None;
            return Some(input);
        }

        let snapped = quantize_device_step(point(step_x, step_y), &mut wheel.carry_dev, scale);
        if snapped.x == 0.0 && snapped.y == 0.0 {
            return None;
        }
        Some(scroll_pixels(
            wheel.position,
            point(px(snapped.x), px(snapped.y)),
            wheel.modifiers,
            TouchPhase::Moved,
        ))
    }

    pub(crate) fn cancel(&mut self, mut emit: impl FnMut(PlatformInput) -> DispatchEventResult) {
        self.touchpad_gesture = None;
        if let Some(wheel) = self.wheel_inertia.take() {
            if wheel.started_emitted {
                emit(scroll_pixels(
                    wheel.position,
                    Point::default(),
                    wheel.modifiers,
                    TouchPhase::Ended,
                ));
            }
        }
        if let Some(momentum) = self.touchpad_momentum.take() {
            emit(scroll_pixels(
                momentum.position,
                Point::default(),
                momentum.modifiers,
                TouchPhase::Ended,
            ));
        }
    }

    fn record_sample(gesture: &mut TouchpadGesture, ts_ns: i64, delta: Point<f32>) {
        let mut sum = Point::<f32>::default();
        for sample in &gesture.samples {
            sum.x += sample.delta.x;
            sum.y += sample.delta.y;
        }
        if (delta.x != 0.0 && sum.x != 0.0 && delta.x.signum() != sum.x.signum())
            || (delta.y != 0.0 && sum.y != 0.0 && delta.y.signum() != sum.y.signum())
        {
            gesture.samples.clear();
            gesture.started_ts_ns = gesture.last_move_ts_ns;
        }

        while gesture.samples.len() > 1
            && gesture
                .samples
                .front()
                .is_some_and(|front| ts_ns.saturating_sub(front.timestamp_ns) > VELOCITY_WINDOW_NS)
        {
            gesture.samples.pop_front();
        }
        gesture.samples.push_back(VelocitySample {
            timestamp_ns: ts_ns,
            delta,
        });
        gesture.last_move_ts_ns = ts_ns;
        gesture.total_distance += delta.x.hypot(delta.y);
    }

    fn estimate_velocity(
        gesture: &TouchpadGesture,
        release_ts_ns: i64,
    ) -> Option<(Point<f32>, f32)> {
        if gesture.samples.is_empty() || gesture.total_distance < MIN_GESTURE_TRAVEL_PX {
            return None;
        }
        if release_ts_ns.saturating_sub(gesture.last_move_ts_ns) > PAUSE_BEFORE_LIFT_NS {
            return None;
        }

        // Recency-weighted velocity estimation so slight finger lift deceleration does not kill coasting.
        let newest_ts = gesture.samples.back()?.timestamp_ns;
        let mut weighted_sum = Point::<f32>::default();
        let mut total_weight = 0.0f32;
        for sample in &gesture.samples {
            let age_sec = (newest_ts.saturating_sub(sample.timestamp_ns) as f32 / 1_000_000_000.0)
                .clamp(0.0, 0.100);
            let w = (-22.0 * age_sec).exp();
            weighted_sum.x += sample.delta.x * w;
            weighted_sum.y += sample.delta.y * w;
            total_weight += w;
        }

        let first_ts = if gesture.samples.len() >= 2 {
            gesture.samples.front()?.timestamp_ns
        } else {
            gesture.started_ts_ns
        };
        let last_ts = newest_ts.max(release_ts_ns.min(gesture.last_move_ts_ns + 12_000_000));
        let raw_dt = ((last_ts.saturating_sub(first_ts)) as f32 / 1_000_000_000.0)
            .clamp(MIN_VELOCITY_DT_SEC, MAX_VELOCITY_DT_SEC);
        let count_f = (gesture.samples.len() as f32).max(1.0);
        let effective_dt = raw_dt * (total_weight / count_f).max(0.25);

        let mut vx = weighted_sum.x / effective_dt;
        let mut vy = weighted_sum.y / effective_dt;
        if vy.abs() > vx.abs() * DOMINANT_AXIS_RATIO {
            vx = 0.0;
        } else if vx.abs() > vy.abs() * DOMINANT_AXIS_RATIO {
            vy = 0.0;
        }

        let speed = vx.hypot(vy);
        if !speed.is_finite() || speed < MIN_TOUCHPAD_FLING_SPEED {
            return None;
        }
        Some((point(vx / speed, vy / speed), speed))
    }
}

/// Quantize a logical-pixel scroll step onto exact whole device pixels at `scale`,
/// preserving sub-device-pixel remainders in `carry_dev` so `gpui-fast`'s integer
/// device-pixel offset snapping never drops or double-rounds fractional HiDPI deltas.
#[inline]
fn quantize_device_step(
    step_logical: Point<f32>,
    carry_dev: &mut Point<f32>,
    scale: f32,
) -> Point<f32> {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let total_dev_x = step_logical.x * s + carry_dev.x;
    let total_dev_y = step_logical.y * s + carry_dev.y;
    let snap_dev_x = total_dev_x.trunc();
    let snap_dev_y = total_dev_y.trunc();
    carry_dev.x = total_dev_x - snap_dev_x;
    carry_dev.y = total_dev_y - snap_dev_y;
    point(snap_dev_x / s, snap_dev_y / s)
}

#[inline]
fn flush_device_step(
    step_logical: Point<f32>,
    carry_dev: &mut Point<f32>,
    scale: f32,
) -> Point<f32> {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let total_dev_x = step_logical.x * s + carry_dev.x;
    let total_dev_y = step_logical.y * s + carry_dev.y;
    carry_dev.x = 0.0;
    carry_dev.y = 0.0;
    point(total_dev_x.round() / s, total_dev_y.round() / s)
}

/// Exact analytical step of a critically-damped 2nd-order system driving `remaining -> 0`:
/// Let `r(t)` be remaining displacement and `v(t) = -r'(t)` be forward scroll velocity.
/// `r(dt) = (r0 + (w * r0 - v0) * dt) * exp(-w * dt)`
/// `step = r0 - r(dt)`
#[inline]
fn step_critically_damped(remaining: f32, velocity: f32, omega: f32, dt: f32) -> (f32, f32, f32) {
    if remaining == 0.0 && velocity == 0.0 {
        return (0.0, 0.0, 0.0);
    }
    let exp_term = (-omega * dt).exp();
    let c1 = remaining;
    let c2 = omega * remaining - velocity;
    let mut next_rem = (c1 + c2 * dt) * exp_term;
    // Clamp against tiny numerical sign flip
    if remaining != 0.0 && next_rem.signum() != remaining.signum() {
        next_rem = 0.0;
    }
    let next_vel = (omega * (c1 + c2 * dt) - c2) * exp_term;
    let step = remaining - next_rem;
    (next_rem, next_vel, step)
}

fn scroll_pixels(
    position: Point<Pixels>,
    delta: Point<Pixels>,
    modifiers: Modifiers,
    touch_phase: TouchPhase,
) -> PlatformInput {
    PlatformInput::Scroll(ScrollEvent {
        position,
        delta: ScrollDelta::Pixels(delta),
        modifiers,
        touch_phase,
        source: ScrollSource::Pointer,
    })
}

fn scroll_lines(
    position: Point<Pixels>,
    delta: Point<f32>,
    modifiers: Modifiers,
    touch_phase: TouchPhase,
) -> PlatformInput {
    PlatformInput::Scroll(ScrollEvent {
        position,
        delta: ScrollDelta::Lines(delta),
        modifiers,
        touch_phase,
        source: ScrollSource::Pointer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touchpad_flick_launches_coasting_and_ends() {
        let mut axis = AxisScroll::default();
        let mut emitted = Vec::new();
        let t0 = Instant::now();
        let pos = point(px(100.0), px(100.0));
        let scale = 1.5;

        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Started,
            Modifiers::default(),
            1_000_000_000,
            scale,
            t0,
            |ev| {
                emitted.push(ev);
                DispatchEventResult::default()
            },
        );
        for step in 1..=4 {
            let now = t0 + Duration::from_millis((step * 8) as u64);
            axis.handle_touchpad(
                pos,
                point(px(0.0), px(-24.0)),
                TouchPhase::Moved,
                Modifiers::default(),
                1_000_000_000 + step * 8_000_000,
                scale,
                now,
                |ev| {
                    emitted.push(ev);
                    DispatchEventResult::default()
                },
            );
            if let Some(ev) = axis.tick(now, scale) {
                emitted.push(ev);
            }
        }
        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Ended,
            Modifiers::default(),
            1_040_000_000,
            scale,
            t0 + Duration::from_millis(40),
            |ev| {
                emitted.push(ev);
                DispatchEventResult::default()
            },
        );

        assert!(axis.has_momentum());
        let mut now = t0 + Duration::from_millis(40);
        let mut coast_frames = 0;
        while axis.has_momentum() {
            now += Duration::from_micros(8333);
            if let Some(PlatformInput::Scroll(ev)) = axis.tick(now, scale) {
                coast_frames += 1;
                if let ScrollDelta::Pixels(d) = ev.delta {
                    // Every emitted step must map onto exact integer device pixels!
                    let dev_y = d.y.as_f32() * scale;
                    assert!(
                        (dev_y - dev_y.round()).abs() < 1e-4,
                        "non-integer device step: {dev_y}"
                    );
                }
                if !axis.has_momentum() {
                    assert_eq!(ev.touch_phase, TouchPhase::Ended);
                }
            }
        }
        assert!(coast_frames > 10);
    }

    #[test]
    fn touchpad_pause_before_lift_stops_without_fling() {
        let mut axis = AxisScroll::default();
        let t0 = Instant::now();
        let pos = point(px(50.0), px(50.0));
        let scale = 2.0;

        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Started,
            Modifiers::default(),
            1_000_000_000,
            scale,
            t0,
            |_| DispatchEventResult::default(),
        );
        axis.handle_touchpad(
            pos,
            point(px(0.0), px(-40.0)),
            TouchPhase::Moved,
            Modifiers::default(),
            1_016_000_000,
            scale,
            t0 + Duration::from_millis(16),
            |_| DispatchEventResult::default(),
        );
        let _ = axis.tick(t0 + Duration::from_millis(16), scale);
        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Ended,
            Modifiers::default(),
            1_106_000_000,
            scale,
            t0 + Duration::from_millis(106),
            |_| DispatchEventResult::default(),
        );
        assert!(!axis.has_momentum());
    }

    #[test]
    fn wheel_notch_glides_stepless_pixels_and_preserves_distance() {
        let mut axis = AxisScroll::default();
        let t0 = Instant::now();
        let pos = point(px(80.0), px(80.0));
        let scale = 1.75;
        let mut total_y = 0.0f32;

        axis.handle_wheel(
            pos,
            point(0.0, -3.0),
            TouchPhase::Moved,
            Modifiers::default(),
            t0,
            |ev| {
                if let PlatformInput::Scroll(ScrollEvent {
                    delta: ScrollDelta::Pixels(d),
                    ..
                }) = ev
                {
                    total_y += d.y.as_f32();
                }
                DispatchEventResult::default()
            },
        );
        assert!(axis.has_momentum());
        let mut now = t0;
        while axis.has_momentum() {
            now += Duration::from_micros(8333);
            if let Some(PlatformInput::Scroll(ScrollEvent {
                delta: ScrollDelta::Pixels(d),
                ..
            })) = axis.tick(now, scale)
            {
                let dev_y = d.y.as_f32() * scale;
                assert!(
                    (dev_y - dev_y.round()).abs() < 1e-4,
                    "wheel step not device-pixel aligned: {dev_y}"
                );
                total_y += d.y.as_f32();
            }
        }
        assert!(
            (total_y - (-3.0 * WHEEL_PIXELS_PER_LINE)).abs() <= (0.5 / scale) + 1e-3,
            "unexpected total_y={total_y}"
        );
    }

    #[test]
    fn touchpad_125hz_vs_120hz_beat_has_zero_dropout_frames_at_1_9_scale() {
        let mut axis = AxisScroll::default();
        let t0 = Instant::now();
        let pos = point(px(200.0), px(200.0));
        let scale = 1.9;

        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Started,
            Modifiers::default(),
            1_000_000_000,
            scale,
            t0,
            |_| DispatchEventResult::default(),
        );

        let mut now = t0;
        let mut ts_ns = 1_000_000_000i64;
        // Simulate worst-case [2 packets, 0 packets, 2 packets, 0 packets] beat between 125Hz & 120Hz
        for frame in 0..8 {
            now += Duration::from_nanos(8_333_333);
            if frame % 2 == 0 {
                for _ in 0..2 {
                    ts_ns += 4_000_000;
                    axis.handle_touchpad(
                        pos,
                        point(px(0.0), px(-12.0)),
                        TouchPhase::Moved,
                        Modifiers::default(),
                        ts_ns,
                        scale,
                        now,
                        |_| DispatchEventResult::default(),
                    );
                }
            }
            let tick_ev = axis.tick(now, scale);
            assert!(
                tick_ev.is_some(),
                "VSync frame {frame} dropped out to 0px during active touchpad beat!"
            );
            if let Some(PlatformInput::Scroll(ScrollEvent {
                delta: ScrollDelta::Pixels(d),
                ..
            })) = tick_ev
            {
                let dev_y = d.y.as_f32() * scale;
                assert!(
                    (dev_y - dev_y.round()).abs() < 1e-4,
                    "non-integer device step at scale=1.9: {dev_y}"
                );
            }
        }
    }
}
