use std::time::{Duration, Instant};

pub(crate) enum IterationEvent<I, R> {
    Input(I),
    Runtime(R),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct IterationOutcome {
    pub input_events: usize,
    pub runtime_events: usize,
    pub exit_code: Option<i32>,
    pub should_draw: bool,
    pub draw_at: Option<Instant>,
}

pub(crate) fn run_event_loop_iteration<I, R, II, RI, Clock, Handler, E>(
    scheduler: &mut FrameScheduler,
    input_events: II,
    runtime_events: RI,
    input_limit: usize,
    runtime_limit: usize,
    frame_time: Clock,
    mut handle_event: Handler,
) -> Result<IterationOutcome, E>
where
    II: IntoIterator<Item = I>,
    RI: IntoIterator<Item = R>,
    Clock: FnOnce() -> Instant,
    Handler: FnMut(IterationEvent<I, R>) -> Result<Option<i32>, E>,
{
    let mut outcome = IterationOutcome::default();
    for event in input_events.into_iter().take(input_limit) {
        scheduler.mark_dirty();
        outcome.input_events += 1;
        if let Some(exit_code) = handle_event(IterationEvent::Input(event))? {
            outcome.exit_code = Some(exit_code);
            return Ok(outcome);
        }
    }
    for event in runtime_events.into_iter().take(runtime_limit) {
        scheduler.mark_dirty();
        outcome.runtime_events += 1;
        if let Some(exit_code) = handle_event(IterationEvent::Runtime(event))? {
            outcome.exit_code = Some(exit_code);
            return Ok(outcome);
        }
    }

    let draw_at = frame_time();
    outcome.should_draw = scheduler.should_draw(draw_at);
    outcome.draw_at = outcome.should_draw.then_some(draw_at);
    Ok(outcome)
}

pub(crate) struct FrameScheduler {
    dirty: bool,
    frame_interval: Duration,
    animation_interval: Duration,
    last_drawn_at: Instant,
    last_animation_at: Instant,
}

impl FrameScheduler {
    pub(crate) fn new(
        now: Instant,
        frame_interval: Duration,
        animation_interval: Duration,
    ) -> Self {
        Self {
            dirty: true,
            frame_interval,
            animation_interval,
            last_drawn_at: now.checked_sub(frame_interval).unwrap_or(now),
            last_animation_at: now,
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn should_draw(&self, now: Instant) -> bool {
        self.dirty && now.duration_since(self.last_drawn_at) >= self.frame_interval
    }

    pub(crate) fn did_draw(&mut self, now: Instant) {
        self.dirty = false;
        self.last_drawn_at = now;
    }

    pub(crate) fn animation_due(&self, now: Instant) -> bool {
        now.duration_since(self.last_animation_at) >= self.animation_interval
    }

    pub(crate) fn did_animate(&mut self, now: Instant) {
        self.last_animation_at = now;
        self.mark_dirty();
    }

    pub(crate) fn poll_timeout(&self, now: Instant, animation_active: bool) -> Duration {
        let frame_wait = if self.dirty {
            self.frame_interval
                .saturating_sub(now.duration_since(self.last_drawn_at))
        } else {
            Duration::MAX
        };
        let animation_wait = if animation_active {
            self.animation_interval
                .saturating_sub(now.duration_since(self.last_animation_at))
        } else {
            Duration::MAX
        };
        frame_wait.min(animation_wait).min(self.frame_interval)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    use super::{FrameScheduler, IterationEvent, run_event_loop_iteration};

    #[test]
    fn bounded_iteration_services_input_runtime_and_draws_at_the_deadline() {
        let started = Instant::now();
        let frame_time = started + Duration::from_millis(16);
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);
        let mut input_queue = (0..100).collect::<VecDeque<_>>();
        let mut runtime_queue = (0..300).collect::<VecDeque<_>>();
        let mut handled_input = Vec::new();
        let mut handled_runtime = Vec::new();

        let outcome = run_event_loop_iteration(
            &mut scheduler,
            std::iter::from_fn(|| input_queue.pop_front()),
            std::iter::from_fn(|| runtime_queue.pop_front()),
            64,
            256,
            || frame_time,
            |event| {
                match event {
                    IterationEvent::Input(value) => handled_input.push(value),
                    IterationEvent::Runtime(value) => handled_runtime.push(value),
                }
                Ok::<Option<i32>, ()>(None)
            },
        )
        .expect("iteration");

        assert_eq!(outcome.input_events, 64);
        assert_eq!(outcome.runtime_events, 256);
        assert_eq!(input_queue.len(), 36);
        assert_eq!(runtime_queue.len(), 44);
        assert_eq!(handled_input, (0..64).collect::<Vec<_>>());
        assert_eq!(handled_runtime, (0..256).collect::<Vec<_>>());
        assert!(outcome.should_draw);

        let mut draw_count = 0;
        if outcome.should_draw {
            draw_count += 1;
            scheduler.did_draw(frame_time);
        }
        assert_eq!(draw_count, 1);
        assert!(!scheduler.should_draw(frame_time));
    }

    #[test]
    fn continuous_input_cannot_starve_a_dirty_frame() {
        let started = Instant::now();
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);

        for millis in [1, 4, 8, 12, 16] {
            scheduler.mark_dirty();
            if millis < 16 {
                assert!(!scheduler.should_draw(started + Duration::from_millis(millis)));
            }
        }

        assert!(scheduler.should_draw(started + Duration::from_millis(16)));
    }

    #[test]
    fn idle_scheduler_does_not_draw_and_waits_for_animation_deadline() {
        let started = Instant::now();
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);

        assert!(!scheduler.should_draw(started + Duration::from_secs(1)));
        assert_eq!(
            scheduler.poll_timeout(started + Duration::from_millis(20), true),
            Duration::from_millis(16)
        );
        assert!(scheduler.animation_due(started + Duration::from_millis(80)));
    }

    #[test]
    fn multiple_dirty_notifications_collapse_into_one_frame() {
        let started = Instant::now();
        let mut scheduler = FrameScheduler::new(
            started,
            Duration::from_millis(16),
            Duration::from_millis(80),
        );
        scheduler.did_draw(started);
        scheduler.mark_dirty();
        scheduler.mark_dirty();

        let frame_time = started + Duration::from_millis(16);
        assert!(scheduler.should_draw(frame_time));
        scheduler.did_draw(frame_time);
        assert!(!scheduler.should_draw(frame_time + Duration::from_millis(16)));
    }
}
