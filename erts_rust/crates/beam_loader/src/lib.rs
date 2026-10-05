//! # `beam_loader` — `.beam` reader & typed-IR lowering front-end
//!
//! Owns the `.beam` **container reader**, the **BEAM instruction decoder** and the
//! **lowering into the typed, block-structured IR** that `beam_engine` interprets.
//! Implements compatibility contract **A.2** (the `.beam` binary/assembly surface,
//! `../../CONTEXT.md` §1) and the Stage-1 loader half of Part II §E/§H (the IR
//! seam, `../CONTEXT.md` §4). This crate is on the critical path of the vertical
//! slice (load + run a real `.beam`).
//!
//! The authoritative design — the frozen public API, the container/chunk wire
//! format, the operand tag/length table and the design decisions — lives in this
//! crate's `CONTEXT.md`; read it before changing anything here.
//!
//! Ground truth (read-only references, never modified):
//! `../../erts/emulator/beam/beam_file.c` (container + operand decoding + function
//! grouping), `../../lib/compiler/src/genop.tab` (the opcode roster), and
//! `../../lib/compiler/src/beam_asm.erl` (the compiler-side writer).
//!
//! ## Status
//!
//! This is the **API skeleton**: every public item of the frozen surface is
//! declared and documented, but the fallible functions that need real logic
//! return `Err(BeamError::BadArg)` and are marked with
//! `// TODO(beam_loader-impl):`. Search that marker for the implementation
//! checklist. The container cursor is `beam_codec::Reader` (the ETF codec's
//! bounds-checked big-endian reader) — do not write a second one.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod chunk;
pub mod container;
pub mod decode;
pub mod error;
pub mod ir;
pub mod load;
pub mod lower;
pub mod opcode;
pub mod operand;

pub use crate::ir::*;
pub use crate::load::{load, load_file};
pub use crate::opcode::*;
pub use crate::operand::*;

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_loader::prelude::*;` brings the frozen IR (`ir::*`), the operand and
/// register identifiers (`operand::*`), the opcode roster (`opcode::*`) and the two
/// entry points ([`load`], [`load_file`]) into scope. Mirrors the crate-root
/// re-exports.
pub mod prelude {
    pub use crate::ir::*;
    pub use crate::load::{load, load_file};
    pub use crate::opcode::*;
    pub use crate::operand::*;
}
