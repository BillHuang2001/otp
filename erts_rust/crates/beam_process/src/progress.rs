//! Thread progress — the single-scheduler stub of the cross-scheduler barrier.
//!
//! The legacy C `ErtsSchedulerData`/`erts_sched_wait_progress` barrier exists so a
//! scheduler can wait until *every* scheduler has seen a generation before, for example,
//! returning memory to the allocator. Milestone-1 runs one scheduler, so there is no other
//! thread to wait for and [`ThreadProgress::wait`] returns immediately; the real
//! multi-scheduler barrier is a documented follow-up (see the crate `CONTEXT.md`, Known
//! Issues).
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_process.h`
//! (`ErtsSchedulerData`), `erl_thr_progress.{c,h}`.

/// The thread-progress generation counter.
pub struct ThreadProgress {
    generation: u64,
}

impl ThreadProgress {
    /// A progress tracker at generation 0.
    pub fn new() -> ThreadProgress {
        ThreadProgress { generation: 0 }
    }

    /// The current generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Advances the generation, returning the new value.
    pub fn bump(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Waits until every scheduler has seen generation `generation`.
    ///
    /// The Milestone-1 single-scheduler stub returns immediately: there is no other
    /// scheduler to wait for.
    pub fn wait(&mut self, _generation: u64) {
        // Under one scheduler, every generation this scheduler has reached is already
        // observed by "all" schedulers (the only one). Nothing to wait for.
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the generation counter and the no-op wait.

    use super::*;

    #[test]
    fn generation_starts_at_zero_and_bumps_monotonically() {
        let mut progress = ThreadProgress::new();
        assert_eq!(progress.generation(), 0);
        assert_eq!(progress.bump(), 1);
        assert_eq!(progress.generation(), 1);
        assert_eq!(progress.bump(), 2);
        assert_eq!(progress.generation(), 2);
    }

    #[test]
    fn wait_is_a_no_op_for_any_generation() {
        let mut progress = ThreadProgress::new();
        progress.wait(0);
        progress.wait(7);
        assert_eq!(progress.generation(), 0, "waiting never advances the generation");
    }
}
