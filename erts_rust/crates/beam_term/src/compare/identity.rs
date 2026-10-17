// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The local-node-aware half of `erts_cmp_compound`: the **mixed
//! internal/external** `pid`/`port`/`ref` arms.
//!
//! An internal pid/port/ref word carries no node — the node is implicit. The C
//! compares such a pair against an *external* one field by field, substituting
//! `erts_this_node` for the internal side's node (`utils.c:1751-1811`,
//! `:2219-2308`, and the shared `pid_common`/`ref_common` tails at
//! `:1777-1811`/`:2262-2287`); with no "this node" available a comparison can only
//! fall back to the class ladder ([`super::class_rank`]). [`LocalNodeId`] is that
//! missing input, and [`super::cmp_with_local_node`] /
//! [`super::cmp_arith_with_local_node`] are the entry points that accept it.
//!
//! # Documented deviation: the sysname is an atom, not a hash
//!
//! `CMP_NODES` (`utils.c:1715-1723`) short-circuits on node *pointer* identity and
//! otherwise orders `(sysname, creation)` with `erts_cmp_atoms` — a lexicographic
//! comparison of the name **bytes** with a length tie-break
//! (`erts/emulator/beam/atom.c`), which no hash preserves. [`LocalNodeId::sysname`]
//! is therefore an atom `Eterm` ordered through the same [`super::cmp_atoms`] path
//! every other atom comparison takes, **not** a hash. This tree has no node
//! registry, so the sysname atom (and the creation) live *inside* the external term
//! (the M1 substitution documented on [`crate::thing::write_external_pid`]); there
//! is no pointer identity to short-circuit on, so both node fields are always
//! compared, in the C's order.

use core::cmp::Ordering;

use super::{cmp_nodes, fallback_cmp, CmpCtx};
use crate::boxed::internal_ref_id;
use crate::eterm::Eterm;
use crate::immediate::{internal_port_data, pid_number, pid_serial};
use crate::ops::TermClass;
use crate::thing::{external_pid_parts, external_port_parts, external_ref_parts};

/// The identity of the runtime's **own** node: the `erts_this_node` a mixed
/// internal/external comparison substitutes for the internal operand's implicit node.
///
/// [`sysname`](Self::sysname) is the node's name as an **atom term** (caller
/// invariant: an atom, as every node sysname is) and [`creation`](Self::creation) is
/// its 32-bit creation number — exactly the two fields `CMP_NODES` compares
/// (`erts/emulator/beam/utils.c:1715-1723`). The name is an atom rather than a hash
/// because the C orders node names by `erts_cmp_atoms`, i.e. lexicographically over
/// the name bytes with a length tie-break — a hash cannot reproduce that. See the
/// module docs.
///
/// Pass it to [`super::cmp_with_local_node`] (exact `=:=` ordering) or
/// [`super::cmp_arith_with_local_node`] (arithmetic `==` ordering); the context-free
/// [`super::cmp`]/[`super::cmp_arith`] keep the class-ladder convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalNodeId {
    /// The local node's sysname as an atom term (`AM_NONODE@NOHOST`-style).
    pub sysname: Eterm,
    /// The local node's creation number (`erts_this_node->creation`).
    pub creation: u32,
}

impl LocalNodeId {
    /// The node named by the sysname atom `sysname` and `creation`.
    ///
    /// The caller must pass an atom term as `sysname`: it is compared through the
    /// atom arm of the comparison, and a non-atom simply orders by its own class
    /// (deterministic, never a panic) rather than as a name.
    pub const fn new(sysname: Eterm, creation: u32) -> Self {
        Self { sysname, creation }
    }
}

/// The mixed-arm dispatcher: `Some(ordering)` when `a`/`b` are an internal and an
/// external word **of the same category** (`pid`, `port` or `ref`, either operand
/// order) *and* a local node is supplied; `None` for every other pair, which then
/// falls through to the class ladder exactly as before.
///
/// # Safety
///
/// `a` and `b` must be live terms classified as `ca`/`cb` — i.e. the caller got those
/// classes from [`crate::ops::class`] on these very words.
pub(super) unsafe fn cmp_mixed(
    a: Eterm,
    b: Eterm,
    ca: TermClass,
    cb: TermClass,
    ctx: CmpCtx<'_>,
) -> Option<Ordering> {
    // Detect the pair *before* the rank shortcut: the C does not use the `_DEF`
    // ladder for two representations of one category.
    let (internal, external, external_first) = match (ca, cb) {
        (TermClass::Pid, TermClass::ExternalPid) => (a, b, false),
        (TermClass::ExternalPid, TermClass::Pid) => (b, a, true),
        (TermClass::Port, TermClass::ExternalPort) => (a, b, false),
        (TermClass::ExternalPort, TermClass::Port) => (b, a, true),
        (TermClass::Ref, TermClass::ExternalRef) => (a, b, false),
        (TermClass::ExternalRef, TermClass::Ref) => (b, a, true),
        _ => return None,
    };
    // Without a local node there is nothing to substitute for the internal side's
    // implicit node, so the no-context convention (the class ladder) still decides.
    let local = ctx.local?;
    let external_class = if external_first { ca } else { cb };
    // SAFETY: `class` classified both words as the category being compared, so each
    // is a live internal/external thing of that category; the arms below read only
    // the fields their own layout owns.
    let ord = unsafe {
        match external_class {
            TermClass::ExternalPid => cmp_mixed_pid(internal, external, local, ctx),
            TermClass::ExternalPort => cmp_mixed_port(internal, external, local, ctx),
            _ => cmp_mixed_ref(internal, external, local, ctx),
        }
    };
    // The arms are written internal-first; a reversed operand order is the reverse
    // verdict (every field comparison below is antisymmetric and total).
    Some(if external_first { ord.reverse() } else { ord })
}

/// An internal pid against an external one — the C's `pid_common`
/// (`erts/emulator/beam/utils.c:1777-1811`), whose `serial`, `number`, node order is
/// reached from the internal/external cases at `:1766-1775` and `:2212-2217`.
///
/// The internal side's node is `local`, exactly the C's `erts_this_node`. When the
/// nodes compare equal the C falls through to `pop_next` — i.e. two pids at the same
/// node with the same serial and number are `Equal`.
///
/// # Safety
///
/// `internal` must be a live internal pid and `external` a live external pid.
unsafe fn cmp_mixed_pid(
    internal: Eterm,
    external: Eterm,
    local: LocalNodeId,
    ctx: CmpCtx<'_>,
) -> Ordering {
    // SAFETY: the caller guarantees a live external pid.
    let (node, number, serial, creation) = unsafe { external_pid_parts(external) };
    // The C compares the `Uint32` fields; the internal pid's serial/number are the
    // same 32-/28-bit fields of its 60-bit payload (`crate::immediate`).
    pid_serial(internal)
        .cmp(&u64::from(serial))
        .then_with(|| pid_number(internal).cmp(&u64::from(number)))
        .then_with(|| cmp_nodes(local.sysname, local.creation, node, creation, ctx))
}

/// An internal port against an external one — the C's `:1751-1764` (internal first)
/// and `:2219-2233` (external first).
///
/// **The numbers are never compared**: the C does `CMP_NODES`, then
/// `ERTS_INTERNAL_ERROR("different nodes compared equal")` — it assumes two local
/// ports cannot be compared in mixed form. Faithfully reproducing that means a
/// differing node decides the port pair on its own, whatever the port numbers are
/// (pinned on the oracle by `tests/compare_class_order.rs`).
///
/// **Documented same-node convention:** for the unreachable-in-C case of equal nodes
/// this compares the internal side's 60-bit `internal_port_data` with the external
/// side's 64-bit `V4_PORT_EXT` number, so the relation stays total, deterministic and
/// antisymmetric instead of panicking on an assertion. Nothing in a well-formed
/// runtime reaches it: an internal port's node is *this* node, and a decoded external
/// port for this node is demoted to the internal form by `beam_codec`.
///
/// # Safety
///
/// `internal` must be a live internal port and `external` a live external port.
unsafe fn cmp_mixed_port(
    internal: Eterm,
    external: Eterm,
    local: LocalNodeId,
    ctx: CmpCtx<'_>,
) -> Ordering {
    // SAFETY: the caller guarantees a live external port.
    let (node, number, creation) = unsafe { external_port_parts(external) };
    let ord = cmp_nodes(local.sysname, local.creation, node, creation, ctx);
    if ord != Ordering::Equal {
        return ord;
    }
    internal_port_data(internal).cmp(&number)
}

/// An internal reference against an external one — the C's `ref_common`
/// (`erts/emulator/beam/utils.c:2239-2287`), reached from the internal-first case at
/// `:2239-2260` and the external-first case at `:2288-2308`.
///
/// Field order: the node first (`:2263`), then the ids as **one big integer**.
///
/// **The M1 internal id as an id array.** This tree's internal reference is the frozen
/// four-word `[header, node = 0, serial = 0, id]` thing whose whole identity is one
/// 60-bit small integer ([`crate::boxed::write_internal_ref`], read back only by
/// [`crate::boxed::internal_ref_id`] — never by the C-layout
/// [`crate::boxed::internal_ref_numbers`]). The external side carries the ERTS
/// `num[]` array with index `len - 1` the most significant, so the id is expressed as
/// the two-word array `[id as u32, (id >> 32) as u32]` — low word at index 0, high
/// word at index 1, i.e. the id's numeric value in the same little-index order ERTS
/// stores its own ref numbers in — and both sides then go through the one shared
/// [`cmp_ref_ids`] (trim the longer side's leading high-index zeros, then compare from
/// the highest index down). Ordering by that array *is* ordering by the integer.
///
/// A reference block whose id cannot be read (a C-layout arity-2 block, or an id word
/// that is not a small integer — only reachable for a forged term) falls back to the
/// deterministic [`fallback_cmp`] rather than inventing an id.
///
/// # Safety
///
/// `internal` must be a live internal reference and `external` a live external
/// reference.
unsafe fn cmp_mixed_ref(
    internal: Eterm,
    external: Eterm,
    local: LocalNodeId,
    ctx: CmpCtx<'_>,
) -> Ordering {
    // SAFETY: the caller guarantees a live external reference.
    let (node, ids, creation) = unsafe { external_ref_parts(external) };
    let ord = cmp_nodes(local.sysname, local.creation, node, creation, ctx);
    if ord != Ordering::Equal {
        return ord;
    }
    // SAFETY: the caller guarantees a live internal reference; the accessor validates
    // the block before reading the id word.
    let Ok(id) = (unsafe { internal_ref_id(internal) }) else {
        return fallback_cmp(internal, external);
    };
    let internal_ids = [id as u32, (id >> 32) as u32];
    cmp_ref_ids(&internal_ids, ids)
}

/// The C's reference-id comparison tail (`erts/emulator/beam/utils.c:2265-2287`): order
/// two non-empty id arrays as one big integer, index `len - 1` most significant, with
/// the longer array's leading (highest-index) **zero** ids trimmed first.
///
/// Shared by the external/external arm ([`super::cmp_external_refs`]) and the
/// internal/external one ([`cmp_mixed_ref`]), so the two cannot drift apart. A
/// non-zero leading id on the longer side decides immediately (that side is the
/// greater one); the surviving ids are then compared from the highest index down.
pub(super) fn cmp_ref_ids(ids_a: &[u32], ids_b: &[u32]) -> Ordering {
    let (mut la, mut lb) = (ids_a.len(), ids_b.len());
    if la > lb {
        // `ids_a[la - 1]` is in bounds because `la > lb >= 0` ⇒ `la >= 1`.
        loop {
            if ids_a[la - 1] != 0 {
                return Ordering::Greater;
            }
            la -= 1;
            if la <= lb {
                break;
            }
        }
    } else if lb > la {
        // `ids_b[lb - 1]` is in bounds because `lb > la >= 0` ⇒ `lb >= 1`.
        loop {
            if ids_b[lb - 1] != 0 {
                return Ordering::Less;
            }
            lb -= 1;
            if la >= lb {
                break;
            }
        }
    }
    debug_assert_eq!(la, lb);
    // Compare the surviving ids from the most significant (highest index) down.
    let mut i = la.min(lb);
    while i > 0 {
        i -= 1;
        if ids_a[i] != ids_b[i] {
            return ids_a[i].cmp(&ids_b[i]);
        }
    }
    Ordering::Equal
}
