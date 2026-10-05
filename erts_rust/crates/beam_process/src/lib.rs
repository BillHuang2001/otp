#![doc = "Processes, scheduler, run queues, thread progress and signal handling."]

//! Erlang processes, the scheduler and the runtime's concurrency plumbing.
//!
//! Implements the process/scheduler half of Part II §A and the Stage-5 row of
//! Part II §H: process control blocks, run queues, scheduled/balanced scheduling,
//! thread progress, exit signals and link/monitor bookkeeping. The scheduler owns
//! the safepoint contract that the execution engine emits.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_process() -> &'static str {
    "beam_process"
}
