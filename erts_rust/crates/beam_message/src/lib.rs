#![doc = "Copy engine (copy_struct) and messaging: send/receive, fragments and message queues."]

//! The copy engine and data-level messaging of the Rust ERTS rewrite (Stage 3).
//!
//! Owns the **copy half of the process memory model** — [`copy_struct`], the iterative copy of a
//! term graph from one process heap into another, and its sharing-preserving sibling
//! [`copy_struct_shared`] — plus the messaging primitives built on it: the [`Message`] envelope,
//! the per-process FIFO [`Queue`] and [`send`]/[`receive`]. Implements Part II §C/§D
//! (immutability bounds what may be shared; everything else is copied) and the Stage-3 row of
//! Part II §H; the legacy `erts/emulator/beam/copy.c` and `erl_message.c` are the ground-truth
//! behaviour.
//!
//! `unsafe` is confined to `copy.rs` (the raw word read/write of the copy loop) and the narrow
//! `layout.rs` accessors; `share.rs`, `message.rs`, `mailbox.rs`, `off_heap.rs`, `node.rs` and
//! `error.rs` are safe Rust.

pub mod copy;
mod error; // pub(crate) copy-failure constructors — NOT public
mod layout; // pub(crate) layout helpers — NOT part of the public API
pub mod mailbox;
pub mod message;
mod node;
pub mod off_heap;
mod share;

pub use crate::copy::{copy_struct, copy_struct_simple, size_of_copy, CopyContext};
pub use crate::mailbox::{receive, receive_ref, send};
pub use crate::message::{Message, Queue, QueueCursor};
pub use crate::node::NodeEntry;
pub use crate::off_heap::{MsoHandle, OffHeapTable};
pub use crate::share::copy_struct_shared;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::copy::{copy_struct, copy_struct_simple, size_of_copy, CopyContext};
    pub use crate::mailbox::{receive, receive_ref, send};
    pub use crate::message::{Message, Queue, QueueCursor};
    pub use crate::node::NodeEntry;
    pub use crate::off_heap::{MsoHandle, OffHeapTable};
    pub use crate::share::copy_struct_shared;
}
