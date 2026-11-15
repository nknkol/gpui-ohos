//! Touchpad coasting momentum and smooth mouse-wheel inertia for ArkUI axis events.
//!
//! On OpenHarmony desktop/2in1 devices, `OH_NativeXComponent`'s UI axis callback
//! (`OH_ArkUI_AxisEvent`) only delivers touchpad scroll deltas while fingers remain in
//! contact (`BEGIN` -> `UPDATE` -> `END`), without post-release coasting packets, and
//! delivers discrete mouse-wheel detents as instantaneous line jumps.
//!
//! [`AxisScroll`] provides window-owned 120Hz frame-driven inertia for both:
//! - **Touchpad (`ScrollDelta::Pixels`)**: 1:1 finger tracking during contact, velocity
//!   estimation over recent hardware timestamps with pause-before-lift detection, and
//!   ArkUI's `exp(-3.15 * t)` friction curve after release (preserving `TouchPhase::Moved`
//!   -> `TouchPhase::Ended` for overscroll bounce).
//! - **Mouse wheel (`ScrollDelta::Lines`)**: smooth exponential ease-out with rapid-spin
//!   velocity accumulation across fractional `ScrollDelta::Lines`, bypassed when zoom/command
//!   modifiers (`Ctrl`/`Super`/`Alt`) are held.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use gpui::{
    DispatchEventResult, Modifiers, Pixels, PlatformInput, Point, ScrollDelta, ScrollEvent,
    ScrollSource, TouchPhase, point, px,
};

const MAX_FLING_SPEED: f32 = 8000.0;
const MIN_TOUCHPAD_FLING_SPEED: f32 = 100.0;
const STOP_SPEED: f32 = 10.0;
// Matches TouchScroll / ArkUI API 12+ friction motion: exp(-4.2 * 0.75 * t).
const TOUCHPAD_DECELERATION: f32 = 4.2 * 0.75;
const MIN_GESTURE_TRAVEL_PX: f32 = 5.0;

// Hardware event timestamps are nanoseconds.
const VELOCITY_WINDOW_NS: i64 = 90_000_000;
const PAUSE_BEFORE_LIFT_NS: i64 = 60_000_000;
const MIN_VELOCITY_DT_SEC: f32 = 0.010;
const MAX_VELOCITY_DT_SEC: f32 = 0.160;
const DOMINANT_AXIS_RATIO: f32 = 3.5;

// Mouse wheel smooth inertia tuning (in fractional ScrollDelta::Lines).
const WHEEL_DECAY_PER_SEC: f32 = 15.0;
const WHEEL_IMMEDIATE_FRACTION: f32 = 0.16;
const WHEEL_STOP_EPSILON_LINES: f32 = 0.012;
const WHEEL_RAPID_SPIN_WINDOW: Duration = Duration::from_millis(110);
const WHEEL_MAX_SPIN_BOOST: f32 = 1.85;
const WHEEL_MAX_REMAINING_LINES: f32 = 60.0;

#[derive(Clone, Copy)]
struct VelocitySample {
    timestamp_ns: i64,
    delta: Point<f32>,
}

struct TouchpadGesture {
    started_ts_ns: i64,
    last_move_ts_ns: i64,
    samples: VecDeque<VelocitySample>,
    total_distance: f32,
}

struct TouchpadMomentum {
    position: Point<Pixels>,
    modifiers: Modifiers,
    direction: Point<f32>,
    speed: f32,
    started_at: Instant,
    duration: Duration,
    emitted_distance: f32,
}

struct WheelInertia {
    position: Point<Pixels>,
    modifiers: Modifiers,
    remaining_lines: Point<f32>,
    last_step_at: Instant,
    last_notch_at: Instant,
    spin_boost: f32,
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
        self.touchpad_momentum.is_some() || self.wheel_inertia.is_some()
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

    pub(crate) fn handle_touchpad(
        &mut self,
        position: Point<Pixels>,
        delta: Point<Pixels>,
        phase: TouchPhase,
        modifiers: Modifiers,
        raw_timestamp_ns: i64,
        now: Instant,
        mut emit: impl FnMut(PlatformInput) -> DispatchEventResult,
    ) {
        self.wheel_inertia = None;
        let ts_ns = self.normalize_timestamp_ns(raw_timestamp_ns, now);
        let delta_f32 = point(delta.x.as_f32(), delta.y.as_f32());
        let nonzero = delta_f32.x != 0.0 || delta_f32.y != 0.0;

        match phase {
            TouchPhase::Started => {
                // Catching an active coasting scroll stops momentum immediately;
                // TouchPhase::Started resets OngoingScroll / ScrollBounce cleanly.
                self.touchpad_momentum = None;
                let mut gesture = TouchpadGesture {
                    started_ts_ns: ts_ns,
                    last_move_ts_ns: ts_ns,
                    samples: VecDeque::with_capacity(16),
                    total_distance: 0.0,
                };
                if nonzero {
                    Self::record_sample(&mut gesture, ts_ns, delta_f32);
                }
                self.touchpad_gesture = Some(gesture);
                emit(scroll_pixels(position, delta, modifiers, TouchPhase::Started));
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
                let gesture = self.touchpad_gesture.get_or_insert_with(|| TouchpadGesture {
                    started_ts_ns: ts_ns,
                    last_move_ts_ns: ts_ns,
                    samples: VecDeque::with_capacity(16),
                    total_distance: 0.0,
                });
                if nonzero {
                    Self::record_sample(gesture, ts_ns, delta_f32);
                    emit(scroll_pixels(position, delta, modifiers, TouchPhase::Moved));
                }
            }
            TouchPhase::Ended => {
                if nonzero {
                    if let Some(gesture) = self.touchpad_gesture.as_mut() {
                        Self::record_sample(gesture, ts_ns, delta_f32);
                    }
                    emit(scroll_pixels(position, delta, modifiers, TouchPhase::Moved));
                }
                let velocity = self
                    .touchpad_gesture
                    .take()
                    .and_then(|gesture| Self::estimate_velocity(&gesture, ts_ns));
                if let Some((direction, speed)) = velocity {
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
                    });
                } else {
                    emit(scroll_pixels(
                        position,
                        Point::default(),
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

        // Preserve discrete ticks when modifier keys are held (e.g. Ctrl+Wheel zoom).
        if modifiers.control || modifiers.platform || modifiers.alt {
            self.wheel_inertia = None;
            if lines.x != 0.0 || lines.y != 0.0 {
                emit(scroll_lines(position, lines, modifiers, phase));
            }
            return;
        }

        if lines.x == 0.0 && lines.y == 0.0 {
            return;
        }

        let inertia = match self.wheel_inertia.as_mut() {
            Some(existing) => {
                if lines.x != 0.0 && lines.x.signum() != existing.remaining_lines.x.signum() {
                    existing.remaining_lines.x = 0.0;
                    existing.spin_boost = 1.0;
                }
                if lines.y != 0.0 && lines.y.signum() != existing.remaining_lines.y.signum() {
                    existing.remaining_lines.y = 0.0;
                    existing.spin_boost = 1.0;
                }
                let since_last = now.saturating_duration_since(existing.last_notch_at);
                if since_last <= WHEEL_RAPID_SPIN_WINDOW {
                    existing.spin_boost = (existing.spin_boost + 0.18).min(WHEEL_MAX_SPIN_BOOST);
                } else {
                    existing.spin_boost = 1.0;
                }
                let boosted = point(lines.x * existing.spin_boost, lines.y * existing.spin_boost);
                existing.remaining_lines.x = (existing.remaining_lines.x + boosted.x)
                    .clamp(-WHEEL_MAX_REMAINING_LINES, WHEEL_MAX_REMAINING_LINES);
                existing.remaining_lines.y = (existing.remaining_lines.y + boosted.y)
                    .clamp(-WHEEL_MAX_REMAINING_LINES, WHEEL_MAX_REMAINING_LINES);
                existing.position = position;
                existing.modifiers = modifiers;
                existing.last_notch_at = now;
                existing.last_step_at = now;
                existing
            }
            None => self.wheel_inertia.insert(WheelInertia {
                position,
                modifiers,
                remaining_lines: point(
                    lines
                        .x
                        .clamp(-WHEEL_MAX_REMAINING_LINES, WHEEL_MAX_REMAINING_LINES),
                    lines
                        .y
                        .clamp(-WHEEL_MAX_REMAINING_LINES, WHEEL_MAX_REMAINING_LINES),
                ),
                last_step_at: now,
                last_notch_at: now,
                spin_boost: 1.0,
            }),
        };

        // Kick off immediately on the input turn so the first frame has zero lag,
        // leaving the rest to decay smoothly across 120Hz VSync frames.
        let immediate = point(
            inertia.remaining_lines.x * WHEEL_IMMEDIATE_FRACTION,
            inertia.remaining_lines.y * WHEEL_IMMEDIATE_FRACTION,
        );
        inertia.remaining_lines.x -= immediate.x;
        inertia.remaining_lines.y -= immediate.y;
        emit(scroll_lines(position, immediate, modifiers, TouchPhase::Moved));
    }

    pub(crate) fn tick(&mut self, now: Instant) -> Option<PlatformInput> {
        if let Some(momentum) = self.touchpad_momentum.as_mut() {
            let elapsed = now
                .saturating_duration_since(momentum.started_at)
                .min(momentum.duration);
            let distance = momentum.speed / TOUCHPAD_DECELERATION
                * (1.0 - (-TOUCHPAD_DECELERATION * elapsed.as_secs_f32()).exp());
            let step = (distance - momentum.emitted_distance).max(0.0);
            momentum.emitted_distance = distance.max(momentum.emitted_distance);
            let ended = elapsed >= momentum.duration;
            let input = scroll_pixels(
                momentum.position,
                point(
                    px(momentum.direction.x * step),
                    px(momentum.direction.y * step),
                ),
                momentum.modifiers,
                if ended {
                    TouchPhase::Ended
                } else {
                    TouchPhase::Moved
                },
            );
            if ended {
                self.touchpad_momentum = None;
            }
            return Some(input);
        }

        let wheel = self.wheel_inertia.as_mut()?;
        let dt = now
            .saturating_duration_since(wheel.last_step_at)
            .as_secs_f32()
            .clamp(1.0 / 240.0, 0.050);
        wheel.last_step_at = now;
        let factor = 1.0 - (-WHEEL_DECAY_PER_SEC * dt).exp();
        let mut step = point(
            wheel.remaining_lines.x * factor,
            wheel.remaining_lines.y * factor,
        );
        wheel.remaining_lines.x -= step.x;
        wheel.remaining_lines.y -= step.y;

        if wheel.remaining_lines.x.hypot(wheel.remaining_lines.y) <= WHEEL_STOP_EPSILON_LINES {
            step.x += wheel.remaining_lines.x;
            step.y += wheel.remaining_lines.y;
            let input = scroll_lines(wheel.position, step, wheel.modifiers, TouchPhase::Ended);
            self.wheel_inertia = None;
            return Some(input);
        }

        Some(scroll_lines(
            wheel.position,
            step,
            wheel.modifiers,
            TouchPhase::Moved,
        ))
    }

    pub(crate) fn cancel(&mut self, mut emit: impl FnMut(PlatformInput) -> DispatchEventResult) {
        self.touchpad_gesture = None;
        self.wheel_inertia = None;
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
        // Reset sample history if the user reverses swipe direction mid-contact.
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

        let mut sum = Point::<f32>::default();
        for sample in &gesture.samples {
            sum.x += sample.delta.x;
            sum.y += sample.delta.y;
        }

        let first_ts = if gesture.samples.len() >= 2 {
            gesture.samples.front()?.timestamp_ns
        } else {
            gesture.started_ts_ns
        };
        let last_ts = gesture.samples.back()?.timestamp_ns.max(release_ts_ns.min(
            gesture.last_move_ts_ns + 16_000_000,
        ));
        let dt_sec = ((last_ts.saturating_sub(first_ts)) as f32 / 1_000_000_000.0)
            .clamp(MIN_VELOCITY_DT_SEC, MAX_VELOCITY_DT_SEC);

        let mut vx = sum.x / dt_sec;
        let mut vy = sum.y / dt_sec;
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

        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Started,
            Modifiers::default(),
            1_000_000_000,
            t0,
            |ev| {
                emitted.push(ev);
                DispatchEventResult::default()
            },
        );
        for step in 1..=4 {
            axis.handle_touchpad(
                pos,
                point(px(0.0), px(-24.0)),
                TouchPhase::Moved,
                Modifiers::default(),
                1_000_000_000 + step * 8_000_000,
                t0 + Duration::from_millis((step * 8) as u64),
                |ev| {
                    emitted.push(ev);
                    DispatchEventResult::default()
                },
            );
        }
        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Ended,
            Modifiers::default(),
            1_040_000_000,
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
            if let Some(PlatformInput::Scroll(ev)) = axis.tick(now) {
                coast_frames += 1;
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

        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Started,
            Modifiers::default(),
            1_000_000_000,
            t0,
            |_| DispatchEventResult::default(),
        );
        axis.handle_touchpad(
            pos,
            point(px(0.0), px(-40.0)),
            TouchPhase::Moved,
            Modifiers::default(),
            1_016_000_000,
            t0 + Duration::from_millis(16),
            |_| DispatchEventResult::default(),
        );
        // Finger held stationary for 90ms before lifting.
        axis.handle_touchpad(
            pos,
            Point::default(),
            TouchPhase::Ended,
            Modifiers::default(),
            1_106_000_000,
            t0 + Duration::from_millis(106),
            |_| DispatchEventResult::default(),
        );
        assert!(!axis.has_momentum());
    }

    #[test]
    fn wheel_notch_glides_and_preserves_total_lines() {
        let mut axis = AxisScroll::default();
        let t0 = Instant::now();
        let pos = point(px(80.0), px(80.0));
        let mut total_y = 0.0f32;

        axis.handle_wheel(
            pos,
            point(0.0, -3.0),
            TouchPhase::Moved,
            Modifiers::default(),
            t0,
            |ev| {
                if let PlatformInput::Scroll(ScrollEvent {
                    delta: ScrollDelta::Lines(d),
                    ..
                }) = ev
                {
                    total_y += d.y;
                }
                DispatchEventResult::default()
            },
        );
        assert!(axis.has_momentum());
        let mut now = t0;
        while axis.has_momentum() {
            now += Duration::from_micros(8333);
            if let Some(PlatformInput::Scroll(ScrollEvent {
                delta: ScrollDelta::Lines(d),
                ..
            })) = axis.tick(now)
            {
                total_y += d.y;
            }
        }
        assert!((total_y - (-3.0)).abs() < 1e-4);
    }
}
