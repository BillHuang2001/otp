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

//! The **local node identity** decode seam: the caller-supplied
//! `erts_this_node` (`sysname` + `creation`) and `local_node_hash` the C decoder
//! compares an incoming identifier / `LOCAL_EXT` wrapper against.
//!
//! The C decoder is not purely wire-driven: several of its checks first ask
//! *"is this identifier's node the node I am running on?"*
//! (`dec_is_this_node(sysname, creation)` == *sysname equals this node's sysname
//! **and** creation equals this node's creation*, `external.c:3123-3129`) and then
//! apply rules that never apply to a remote node. This codec owns no node
//! registry, so those rules were previously unreachable (documented divergences —
//! see the crate `CONTEXT.md`, §Known Issues). This module adds the **additive**
//! seam that lets the caller (`beam_rt`/`beam_dist`, which own node identity)
//! install that identity; with nothing installed every check is inert and decoding
//! is byte-for-byte what it was before.
//!
//! ## The three identity-gated rules
//!
//! * **D2 — local reference first number.** `NEWER_REFERENCE_EXT` (`90`) has no
//!   *pre-node* bound on its first number, but `ref_ext_common` re-checks
//!   `r0 >= MAX_REFERENCE` inside its **local-node** branch
//!   (`external.c:4804-4813`; the magic-ref table relies on 18 bits): a *remote*
//!   node keeps any `r0`, a local one is `BadArg`. (`REFERENCE_EXT` (101) and
//!   `NEW_REFERENCE_EXT` (114) enforce the same bound unconditionally, *before*
//!   the node is resolved — `external.c:4757`/`:4782` — and are unchanged here.)
//!   The local-node **demotion** of a reference — the C's
//!   `ref_ext_common` internal-`ErtsORefThing` branch, `external.c:4821-4862` — is
//!   representability-gated: the internal form's id slot holds one 60-bit small, so
//!   an image whose packed id does not fit keeps the faithful **external** reference
//!   form (`crate::ref_wire::representable_ref_id`), never a truncated id and never
//!   an error.
//! * **D6 — local pid/port bounds.** `dec_pid` rejects a pid whose node is the
//!   local node with `number > ERTS_MAX_INTERNAL_PID_NUMBER` or
//!   `serial > ERTS_MAX_INTERNAL_PID_SERIAL` (`external.c:3175-3179`); the port arm
//!   rejects a local port with `num > ERTS_MAX_INTERNAL_PORT_NUMBER`
//!   (`external.c:4718-4721`). Both apply to *every* wire form of the kind, and to
//!   the `NEW_FUN_EXT` creator pid (which the C decodes through the same `dec_pid`,
//!   `external.c:5153-5160`).
//! * **D4 — the `LOCAL_EXT` node hash.** A `LOCAL_EXT` (`121`) image carries a
//!   4-byte node hash (`external.c:6437-6512`; verified by `binary_to_term/1,2`,
//!   which always runs the sizing pass) that must match the running node's, else
//!   `badarg`.
//!
//! ## The hash is a caller-supplied value, not an algorithm
//!
//! The C's `local_node_hash` is a **per-VM-start** value (hostname + OS pid + time
//! offset + monotonic time, `external.c:136-199`), and the 4 bytes a `LOCAL_EXT`
//! image carries are `erts_block_hash(<inner term bytes>, local_node_hash)`
//! (`external.c:6471-6512` encode, `:4067-4105` encode). The codec owns neither the
//! seed nor a node identity, so — exactly like the **encoder** side, whose
//! `encode_term_local`/`finish_local` already take the hash as an opaque
//! caller-supplied `u32` — this seam takes the **expected hash value** and compares
//! it against the image's 4 bytes. It is therefore the caller's job to supply the
//! value its own `LOCAL_EXT` images carry (a value the codec cannot derive).
//! Installed as [`Decoder::with_local_node_hash`].
//!
//! ## `LOCAL_EXT`-wrapped identifiers and the creator pid (D7)
//!
//! `[local]` makes the C replace an *internal* identifier's node atom with the
//! single byte `NIL_EXT` and force its creation to `0` (`ERTS_DIST_EXT_INTERNAL_NC`,
//! `external.c:2978`/`:3651`/`:3686`), which is why a `NEW_FUN_EXT` may carry a
//! `LOCAL_EXT`-wrapped creator pid. The C decodes that creator through the generic
//! `dec_term`, so a `[121][hash:4][88 …]` creator is accepted (and its hash is
//! verified by the sizing pass); this module teaches [`super::fun`]'s
//! `decode_creator_pid` to do the same.

use beam_common::Result;
use beam_term::prelude::{atom_val, is_atom, is_nil};
use beam_term::{Atom, Eterm};

use crate::error::bad_arg;

use super::{Decoder, MAX_REFERENCE};

/// `ERTS_MAX_INTERNAL_PID_NUMBER` = `(1 << _PID_NUM_SIZE) - 1` = `2^28 - 1`
/// (`erl_node_container_utils.h:182`, `erl_term.h:646`).
pub(super) const MAX_INTERNAL_PID_NUMBER: u64 = (1u64 << beam_term::immediate::PID_NUM_SIZE) - 1;

/// `ERTS_MAX_INTERNAL_PID_SERIAL` = `(1 << _PID_SER_SIZE) - 1` = `2^32 - 1`
/// (`erl_node_container_utils.h:183`, `erl_term.h:645`). A wire serial is a
/// `u32`, so this bound is unreachable — it is kept to mirror the C exactly.
pub(super) const MAX_INTERNAL_PID_SERIAL: u64 = (1u64 << beam_term::immediate::PID_SER_SIZE) - 1;

/// `ERTS_MAX_INTERNAL_PORT_NUMBER` = `(1 << _PORT_NUM_SIZE) - 1` = `2^60 - 1`
/// (`erl_node_container_utils.h:250`, `erl_term.h:694`). Reachable through the
/// 64-bit `V4_PORT_EXT` id; the 32-bit port tags can never exceed it.
pub(super) const MAX_INTERNAL_PORT_NUMBER: u64 =
    (1u64 << beam_term::immediate::PORT_DATA_SIZE) - 1;

impl<'a> Decoder<'a> {
    /// Installs the local node's identity — its `sysname` atom and `creation`
    /// (`erts_this_node->sysname` / `->creation`) — so the identity-gated decode
    /// rules (D2/D6, §the module doc) apply.
    ///
    /// `node` is the `sysname` atom **as interned in the caller's
    /// [`AtomTable`](beam_term::AtomTable)** — the same atom the wire image carries
    /// for the local node (the caller resolves it once, e.g. from `node()`). An
    /// incoming identifier whose node atom and creation both match is treated as
    /// local; anything else keeps the pre-seam behaviour.
    ///
    /// Additive: without a call to this method (or [`Decoder::with_local_node_hash`])
    /// decoding is unchanged, so no rejection can be introduced by accident.
    pub fn with_local_node(self, node: Atom, creation: u32) -> Decoder<'a> {
        Decoder { local_node: Some((node, creation)), ..self }
    }

    /// Installs the **expected `LOCAL_EXT` (`121`) node hash** (§the module doc):
    /// the 4 bytes an image this node accepts must carry.
    ///
    /// With it installed a `LOCAL_EXT` wrapper — top-level, nested in a term, or
    /// wrapping a `NEW_FUN_EXT` creator pid — whose 4-byte hash differs yields
    /// `BeamError::BadArg`, matching the oracle's `decoded_size` verification
    /// (`external.c:6505-6511`). Without it the 4 bytes are consumed and ignored,
    /// exactly as before.
    ///
    /// The value is opaque to the codec (see §the module doc): it is the same kind
    /// of caller-supplied hash [`crate::encode_term_local`] writes.
    pub fn with_local_node_hash(self, hash: u32) -> Decoder<'a> {
        Decoder { local_hash: Some(hash), ..self }
    }

    /// Whether an identifier's node/creation identify the local node
    /// (`dec_is_this_node`, `external.c:3123-3129`).
    ///
    /// Two ways to match, exactly as the C:
    ///
    /// * the `[local]` internal-node marker (`INTERNAL_LOCAL_SYSNAME` = `NIL`,
    ///   written only under `ERTS_DIST_EXT_INTERNAL_NC`) matches **unconditionally** —
    ///   no sysname/creation comparison and no installed identity. `node` can only be
    ///   that marker inside [`Decoder::internal_node`] mode (an ordinary node field
    ///   must decode to an atom), so nothing changes without a `LOCAL_EXT` wrapper.
    /// * otherwise the installed local node identity ([`Decoder::with_local_node`])
    ///   must match both the sysname atom and the creation.
    ///
    /// `false` whenever neither applies, so every caller of this predicate is inert
    /// without a `LOCAL_EXT` wrapper or an installed identity.
    pub(super) fn is_local_node(&self, node: Eterm, creation: u32) -> bool {
        if self.is_internal_node_marker(node) {
            return true;
        }
        match self.local_node {
            Some((local, local_creation)) => {
                is_atom(node) && atom_val(node) == local && creation == local_creation
            }
            None => false,
        }
    }

    /// Whether `node` is the `[local]`/ETS **internal-node marker**
    /// (`INTERNAL_LOCAL_SYSNAME` = `NIL`, `external.c:2965`) — only ever possible
    /// inside [`Decoder::internal_node`] mode, i.e. after a `LOCAL_EXT` (`121`)
    /// wrapper (`dec_sysname`, `external.c:3114-3122`).
    pub(super) fn is_internal_node_marker(&self, node: Eterm) -> bool {
        self.internal_node && is_nil(node)
    }

    /// D2: rejects a `NEWER_REFERENCE_EXT` first number at or past
    /// [`MAX_REFERENCE`] **when the node is the local node** (`external.c:4807`).
    pub(super) fn check_local_ref_first_number(
        &self,
        node: Eterm,
        creation: u32,
        r0: u32,
    ) -> Result<()> {
        if r0 >= MAX_REFERENCE && self.is_local_node(node, creation) {
            return Err(bad_arg());
        }
        Ok(())
    }

    /// D6: rejects a pid whose node is the local node with a process number past
    /// [`MAX_INTERNAL_PID_NUMBER`] or a serial past [`MAX_INTERNAL_PID_SERIAL`]
    /// (`dec_pid`, `external.c:3175-3179`).
    pub(super) fn check_local_pid(
        &self,
        node: Eterm,
        creation: u32,
        number: u32,
        serial: u32,
    ) -> Result<()> {
        if self.is_local_node(node, creation)
            && (u64::from(number) > MAX_INTERNAL_PID_NUMBER
                || u64::from(serial) > MAX_INTERNAL_PID_SERIAL)
        {
            return Err(bad_arg());
        }
        Ok(())
    }

    /// D6: rejects a port whose node is the local node with an id past
    /// [`MAX_INTERNAL_PORT_NUMBER`] (`external.c:4718-4721`).
    pub(super) fn check_local_port(
        &self,
        node: Eterm,
        creation: u32,
        number: u64,
    ) -> Result<()> {
        if number > MAX_INTERNAL_PORT_NUMBER && self.is_local_node(node, creation) {
            return Err(bad_arg());
        }
        Ok(())
    }

    /// D4: rejects a `LOCAL_EXT` node hash that disagrees with the installed one
    /// (`external.c:6505-6511`). Inert without [`Decoder::with_local_node_hash`].
    pub(super) fn check_local_node_hash(&self, hash: u32) -> Result<()> {
        match self.local_hash {
            Some(expected) if expected != hash => Err(bad_arg()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the seam's helpers: the node-identity predicate, the D2/D6
    //! bound checks and the hash comparison — each inert with nothing installed.
    //!
    //! Tests are allowed to `unwrap` (a panic there is a test failure); the
    //! production bodies above never do.

    use super::*;
    use beam_term::prelude::{make_atom, make_small};

    /// A decoder over an empty wire, with the seam installed by the caller.
    fn bare() -> Decoder<'static> {
        Decoder::new_body(&[])
    }

    #[test]
    fn nothing_installed_is_inert() {
        let decoder = bare();
        let node = make_atom(Atom::from_raw(1));
        assert!(!decoder.is_local_node(node, 0));
        assert!(decoder.check_local_ref_first_number(node, 0, u32::MAX).is_ok());
        assert!(decoder.check_local_pid(node, 0, u32::MAX, u32::MAX).is_ok());
        assert!(decoder.check_local_port(node, 0, u64::MAX).is_ok());
        assert!(decoder.check_local_node_hash(u32::MAX).is_ok());
        // A non-atom node never matches either.
        let identity = Decoder::new_body(&[]).with_local_node(Atom::from_raw(1), 0);
        assert!(!identity.is_local_node(make_small(1), 0));
    }

    #[test]
    fn identity_gates_every_check() {
        let local = Atom::from_raw(3);
        let node = make_atom(local);
        let decoder = bare().with_local_node(local, 7);

        // Matching node + creation.
        assert!(decoder.is_local_node(node, 7));
        assert!(decoder.check_local_ref_first_number(node, 7, MAX_REFERENCE).is_err());
        let max_pid_number = u32::try_from(MAX_INTERNAL_PID_NUMBER).unwrap();
        assert!(decoder.check_local_pid(node, 7, max_pid_number, 0).is_ok());
        assert!(decoder.check_local_pid(node, 7, max_pid_number + 1, 0).is_err());
        assert!(decoder.check_local_port(node, 7, MAX_INTERNAL_PORT_NUMBER).is_ok());
        assert!(decoder.check_local_port(node, 7, MAX_INTERNAL_PORT_NUMBER + 1).is_err());

        // A different creation, or a different node atom, is *not* local: the same
        // shapes must pass.
        assert!(!decoder.is_local_node(node, 8));
        assert!(decoder.check_local_ref_first_number(node, 8, u32::MAX).is_ok());
        assert!(decoder.check_local_pid(node, 8, u32::MAX, u32::MAX).is_ok());
        assert!(decoder.check_local_port(node, 8, u64::MAX).is_ok());
        let remote = make_atom(Atom::from_raw(4));
        assert!(!decoder.is_local_node(remote, 7));
        assert!(decoder.check_local_pid(remote, 7, u32::MAX, u32::MAX).is_ok());
    }

    #[test]
    fn hash_comparison_is_exact() {
        let decoder = bare().with_local_node_hash(0xDEAD_BEEF);
        assert!(decoder.check_local_node_hash(0xDEAD_BEEF).is_ok());
        assert!(decoder.check_local_node_hash(0).is_err());
        assert!(decoder.check_local_node_hash(0xDEAD_BEEE).is_err());
    }

    #[test]
    fn serial_bound_is_kept() {
        // `MAX_INTERNAL_PID_SERIAL` is `2^32 - 1`, so no `u32` wire serial can
        // exceed it — the check mirrors the C without ever firing.
        assert_eq!(MAX_INTERNAL_PID_SERIAL, u64::from(u32::MAX));
        let local = Atom::from_raw(1);
        let decoder = bare().with_local_node(local, 0);
        assert!(decoder.check_local_pid(make_atom(local), 0, 0, u32::MAX).is_ok());
    }
}
