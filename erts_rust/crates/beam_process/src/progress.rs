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
//
// TODO(impl): implement `ThreadProgress` (all bodies are stubs).
pub struct ThreadProgress {
    generation: u64,
}

impl ThreadProgress {
    /// A progress tracker at generation 0.
    pub fn new() -> ThreadProgress {
        unimplemented!("stub — to be implemented")
    }

    /// The current generation.
    pub fn generation(&self) -> u64 {
        unimplemented!("stub — to be implemented")
    }

    /// Advances the generation, returning the new value.
    pub fn bump(&mut self) -> u64 {
        unimplemented!("stub — to be implemented")
    }

    /// Waits until every scheduler has seen generation `generation`.
    ///
    /// The Milestone-1 single-scheduler stub returns immediately: there is no other
    /// scheduler to wait for.
    pub fn wait(&mut self, generation: u64) {
        unimplemented!("stub — to be implemented")
    }
}
