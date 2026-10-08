//! The register files: the x-register file and the float-register (FR) file.
//!
//! [`MAX_REGISTER`] and [`MAX_FLOAT_REGISTER`] bound the two files;
//! [`Machine::reset`] restores them (and the rest of the machine state) to its
//! initial, NIL-filled state. The accessors ([`Machine::x`]/[`Machine::set_x`]/
//! [`Machine::get_fr`]/[`Machine::set_fr`]) are bounds-checked, and an
//! out-of-range write is the typed [`crate::error::bad_register`] error — never a
//! panic and never a silently dropped write.

use beam_common::Result;
use beam_term::Eterm;

use crate::error;

use super::catch::UNRESOLVED_CLASS_ATOMS;
use super::Machine;

/// X-register file size (the `beam_emu.c` `MAX_REG` bound).
pub const MAX_REGISTER: u32 = 1024;

/// Float-register (FR) file size.
///
/// The C runtime's float-register file is
/// `union { FloatDef d[MAX_REG]; ... } f_reg_array`
/// (`../../erts/emulator/beam/erl_process.h`) with `#define MAX_REG 1024`
/// (`../../erts/emulator/beam/erl_vm.h:49`), so the FR file mirrors the real FR
/// count (1024) — the same bound as [`MAX_REGISTER`]. The C JIT bounds the index
/// to `0..=1023` (`../../erts/emulator/beam/jit/x86/beam_asm.hpp`).
pub const MAX_FLOAT_REGISTER: u32 = 1024;

/// The all-[`NIL`](beam_term::NIL) template the register files are refilled from.
///
/// [`Machine::reset`] rewrites the whole x-register file (`max_register` words) and
/// the whole FR file ([`MAX_FLOAT_REGISTER`] words) on **every** run, so the refill
/// is on the per-run critical path: `memcpy` out of this static — which is small
/// enough to stay resident in L1 and is copied by the platform's optimized
/// `memcpy` — is measurably (≈2.5x) faster than a same-shaped `fill`/store loop
/// compiled at the crate's own optimization level, and it is exactly as safe: the
/// destination is overwritten word-for-word with [`beam_term::NIL`], the very value
/// `fill` would have stored.
///
/// It is deliberately **not** a `Machine` field: a per-machine buffer would double
/// the memory traffic (one copy into the buffer, one out of it) and would have to be
/// allocated, whereas a shared read-only table is cached once and read forever.
static NIL_FILE: [Eterm; MAX_REGISTER as usize] = [Eterm::nil(); MAX_REGISTER as usize];

/// Overwrite every slot of `slots` with [`beam_term::NIL`].
///
/// A `memcpy` from [`NIL_FILE`], chunked so any slice length (in particular a
/// register file made larger than [`MAX_REGISTER`] by
/// [`Machine::with_max_register`]) is handled without panicking; the common case —
/// a file no larger than [`MAX_REGISTER`] — is a single `memcpy`.
#[inline]
fn fill_nil(slots: &mut [Eterm]) {
    let mut rest = slots;
    while !rest.is_empty() {
        let take = rest.len().min(NIL_FILE.len());
        let (head, tail) = rest.split_at_mut(take);
        head.copy_from_slice(&NIL_FILE[..take]);
        rest = tail;
    }
}
impl Machine {
    /// Read X register `n`, or `None` if it is outside the register file.
    ///
    /// `#[inline]`-marked: this is the operand seam's innermost accessor, called
    /// several times per interpreted instruction, and the crate is compiled into
    /// many codegen units, so without the hint the call never reaches the dispatch
    /// loop's own unit.
    #[inline]
    pub fn x(&self, n: u32) -> Option<Eterm> {
        self.x.get(n as usize).copied()
    }

    /// Read float register `n`, or `None` if it is outside the FR file.
    ///
    /// Mirrors [`Machine::x`]: a bounds-checked read of the FR file.
    #[inline]
    pub fn get_fr(&self, n: u32) -> Option<Eterm> {
        self.fr.get(n as usize).copied()
    }

    /// Write `value` into float register `n`.
    ///
    /// Mirrors [`Machine::set_x`]: returns [`crate::error::bad_register`] if `n` is
    /// outside the FR file.
    #[inline]
    pub fn set_fr(&mut self, n: u32, value: Eterm) -> Result<()> {
        match self.fr.get_mut(n as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(error::bad_register(n)),
        }
    }

    /// Write `v` into X register `n`.
    ///
    /// Returns [`crate::error::bad_register`] if `n` is outside the register file.
    #[inline]
    pub(crate) fn set_x(&mut self, n: u32, v: Eterm) -> Result<()> {
        match self.x.get_mut(n as usize) {
            Some(slot) => {
                *slot = v;
                Ok(())
            }
            None => Err(error::bad_register(n)),
        }
    }

    /// Reset to the initial state: a fresh (NIL-filled) register file and empty
    /// stack and frame stacks.
    ///
    /// # Cost model
    ///
    /// This is on the per-run critical path (every `run_*` entry resets first), so
    /// it is written as the cheapest *correct* full reset:
    ///
    /// * the register/FR files are **refilled in one pass** by [`fill_nil`] (a
    ///   `memcpy` of [`NIL_FILE`]); a `clear()` followed by a `resize()` writes the
    ///   very same words but through a store loop rather than the platform's
    ///   optimized `memcpy` (measured ≈2.5x slower for the two files together);
    /// * a capacity that is already correct is **never reallocated** — the vectors
    ///   are only resized when their length differs from the target (which happens
    ///   once, for the first reset after `Machine::with_max_register`, and never
    ///   again in steady state), so the register base stays put across runs;
    /// * no dirty high-water-mark bookkeeping: a **skipped** clear would leak a
    ///   stale term into the next run, because the baseline JIT writes
    ///   `machine.x` directly through raw pointers from generated code and can
    ///   never report which registers it touched. The whole file is rewritten on
    ///   every reset, so `x`/`fr` are exactly `NIL`-filled for the next run no
    ///   matter what wrote them.
    #[inline]
    pub fn reset(&mut self) {
        let n = self.max_register as usize;
        if self.x.len() != n {
            self.x.clear();
            self.x.resize(n, Eterm::default());
        }
        fill_nil(&mut self.x);

        let fr_len = MAX_FLOAT_REGISTER as usize;
        if self.fr.len() != fr_len {
            self.fr.clear();
            self.fr.resize(fr_len, Eterm::default());
        }
        fill_nil(&mut self.fr);

        self.stack.clear();
        self.frames.clear();
        self.catch_frames.clear();
        self.pending_exception = None;
        // The class atoms belong to the module being run, so a reset (the start of
        // every `run_*`) invalidates them; the exceptions family reinstalls them.
        self.class_atoms = UNRESOLVED_CLASS_ATOMS;
        // Likewise the literal area: it belongs to the module being run and is
        // re-bound by `run_loop`.
        self.literals = None;
        // The execution-position introspection belongs to the run that is starting:
        // `step_instruction` re-records `current_code` and `run_loop` re-records
        // `entry_call` (the `$initial_call` analogue).
        self.current_code = None;
        self.entry_call = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{Machine, MAX_FLOAT_REGISTER, MAX_REGISTER};
    use beam_term::Eterm;

    #[test]
    fn new_has_nil_filled_register_file() {
        let machine = Machine::new();
        assert_eq!(machine.x.len(), MAX_REGISTER as usize);
        assert_eq!(machine.x(0), Some(Eterm::default()));
        assert_eq!(machine.x(MAX_REGISTER - 1), Some(Eterm::default()));
        assert_eq!(machine.x(MAX_REGISTER), None);
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
    }

    #[test]
    fn with_max_register_bounds_the_file() {
        let machine = Machine::with_max_register(4);
        assert_eq!(machine.x, vec![Eterm::default(); 4]);
    }

    #[test]
    fn new_has_nil_filled_float_register_file() {
        let machine = Machine::new();
        assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
        assert_eq!(machine.get_fr(0), Some(Eterm::default()));
        assert_eq!(
            machine.get_fr(MAX_FLOAT_REGISTER - 1),
            Some(Eterm::default())
        );
        assert_eq!(machine.get_fr(MAX_FLOAT_REGISTER), None);
    }

    #[test]
    fn set_fr_get_fr_round_trip_in_range() {
        let mut machine = Machine::new();
        let v = Eterm::from_raw(0x5F5F);
        assert!(machine.set_fr(10, v).is_ok());
        assert_eq!(machine.get_fr(10), Some(v));
        assert_eq!(machine.get_fr(11), Some(Eterm::default()));
    }

    #[test]
    fn fr_access_is_bounds_checked() {
        let mut machine = Machine::new();
        assert_eq!(machine.get_fr(MAX_FLOAT_REGISTER), None);
        let err = machine.set_fr(MAX_FLOAT_REGISTER, Eterm::from_raw(1));
        assert!(matches!(err, Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    fn reset_clears_a_dirty_float_register() {
        let mut machine = Machine::new();
        machine.set_fr(3, Eterm::from_raw(0x77)).unwrap();
        machine.set_x(0, Eterm::from_raw(0x88)).unwrap();
        machine.reset();
        assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
        assert_eq!(machine.get_fr(3), Some(Eterm::default()));
        // The x-register behavior is untouched by the FR reset.
        assert_eq!(machine.x.len(), MAX_REGISTER as usize);
        assert_eq!(machine.x(0), Some(Eterm::default()));
    }

    #[test]
    fn set_x_checks_the_bound() {
        let mut machine = Machine::with_max_register(4);
        let v = Eterm::from_raw(0x2A);
        assert!(machine.set_x(3, v).is_ok());
        assert_eq!(machine.x(3), Some(v));
        assert!(machine.set_x(4, v).is_err());
    }

    #[test]
    fn reset_clears_state_and_refills_registers() {
        let mut machine = Machine::with_max_register(3);
        machine.set_x(0, Eterm::from_raw(7)).unwrap();
        machine.push_frame(2).unwrap();
        machine.reset();
        assert_eq!(machine.x, vec![Eterm::default(); 3]);
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
    }

    #[test]
    #[allow(unsafe_code)]
    fn reset_clears_a_register_written_through_a_raw_pointer() {
        // The baseline JIT writes `machine.x` DIRECTLY through the raw pointer the
        // generated code holds, so it can never notify a dirty tracker. This test
        // stands in for that write: a raw store into register 500 must still be
        // wiped by the next `reset`, i.e. no high-water-mark shortcut may skip it.
        let mut machine = Machine::new();
        // SAFETY: the store stays inside the register file's allocation
        // (`MAX_REGISTER` live words) and `Eterm` is `repr(transparent)` over a
        // machine word, so the write is in bounds and well-aligned.
        unsafe {
            let base = machine.x.as_mut_ptr();
            base.add(500).write(Eterm::from_raw(0xDEAD_BEEF));
        }
        assert_eq!(machine.x(500), Some(Eterm::from_raw(0xDEAD_BEEF)));
        machine.reset();
        assert_eq!(machine.x(500), Some(Eterm::default()));
        assert!(machine.x.iter().all(|slot| *slot == Eterm::default()));
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
    }

    #[test]
    fn repeated_resets_never_reallocate_the_register_files() {
        // The reset must retain capacity across runs: a `Machine` is reused for
        // every run, and a reallocation would both cost time and move the register
        // base under any live raw pointer.
        let mut machine = Machine::with_max_register(8);
        let x_capacity = machine.x.capacity();
        let fr_capacity = machine.fr.capacity();
        assert!(x_capacity >= 8);
        assert!(fr_capacity >= MAX_FLOAT_REGISTER as usize);
        for _ in 0..16 {
            machine.set_x(0, Eterm::from_raw(1)).unwrap();
            machine.set_fr(0, Eterm::from_raw(1)).unwrap();
            machine.reset();
            assert_eq!(machine.x.capacity(), x_capacity);
            assert_eq!(machine.fr.capacity(), fr_capacity);
            assert_eq!(machine.x.len(), 8);
            assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        }
    }
}
