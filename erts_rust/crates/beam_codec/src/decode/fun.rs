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

//! The fun / export tag arms of [`Decoder`]: `EXPORT_EXT` (`113`) and
//! `NEW_FUN_EXT` (`112`).
//!
//! Both tags are handled **inside the shared term walker**
//! ([`Decoder::decode`]) — the `match tag` dispatch stays in the parent
//! [`crate::decode`] module — so a fun nested in a tuple/list/map decodes like any
//! other sub-term (a `.beam` `LitT` literal is exactly that shape). The helpers live
//! here as a second `impl` block on the decoder; being a child of the `decode`
//! module they reach its private wire cursor and node-atom helper directly.
//!
//! This module is safe Rust, like its parent.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{atom_val, is_small, signed_value, FunPid, MAX_ARG};
use beam_term::{AtomTable, Eterm};

use crate::error::{bad_arg, unsupported};
use crate::tag::Tag;

use super::Decoder;

impl<'a> Decoder<'a> {
    /// Decodes `EXPORT_EXT` (`113`): `[module atom][function atom][arity term]`
    /// (`external.c:5006-5040`).
    ///
    /// The module and function atoms are decoded **in stream order** (so atom
    /// interning follows the wire order) and the arity is a *normal* decoded term:
    /// the C requires it to be a small (`is_small(temp)`) whose value is `>= 0`
    /// (`external.c:5025-5031`), so a negative or non-small arity is `BadArg`. The
    /// result is a [`FunKind::External`](crate::FunKind) fun with an empty
    /// environment — the inlined-descriptor replacement for the C `Export`
    /// (`num_free == 0`).
    ///
    /// **The arity is also bounded by a 32-bit field.** The C reads the arity as a
    /// 64-bit `Sint` (`Sint arity = signed_val(temp);`, `external.c:5028`) and passes
    /// it to `erts_export_get_or_make_stub(Eterm, Eterm, unsigned)`
    /// (`export.h:137`), so it is truncated to its low 32 bits before it is stored in
    /// `ErtsCodeMFA.arity`, a `Uint` (`code_ix.h:89-97`) — the same 32-bit width as
    /// `ErlFunEntry.arity` (`erl_fun.h:43`). The encoder then reads those 32 bits back
    /// (`external.c:4000`/`:5912`). So `2^32` decodes to arity 0, `2^32 + 7` to 7 and
    /// `3 * 2^31` to `0x8000_0000` (verified against erts-17.1), and this decoder
    /// truncates the same way rather than keeping the full 60-bit small.
    ///
    /// **The arity is not bounded by the 8-bit fun-header field.** In C the true
    /// arity never lives in the term: the fun is the shared `Export` lambda built by
    /// `create_shared_lambda` (`export.c:46-58`), whose header field is 8 bits — so
    /// `fun_arity/1` masks it (`erl_fun.h:92-93`) and `fun_info(F, arity)` reports
    /// `arity & 0xFF` (verified against erts-17.1: 256 ⇒ `{arity,0}`, 257 ⇒ 1,
    /// 511 ⇒ 255, 512 ⇒ 0) — while the *encoder* reads the full arity back out of
    /// the `Export` table (`external.c:5038`) and re-encodes it unharmed. This tree
    /// inlines the `Export`'s `info.mfa.arity` into descriptor word 3
    /// ([`beam_term::fun::write_fun_external`]: `External` = `[header, module,
    /// function, arity]`, 4 words), so [`crate::term::fun_true_arity_term`] reports
    /// the true value the encoder emits while [`crate::term::fun_arity_term`] keeps
    /// the header mask — the same split as the oracle. Every non-negative arity is
    /// therefore materialized faithfully (modulo the 32-bit truncation above): there
    /// is no `SystemLimit` on this path.
    pub(super) fn decode_export(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
        let module = self.decode_node_atom(heap, atoms)?;
        let function = self.decode_node_atom(heap, atoms)?;
        let arity = self.decode(heap, atoms)?;
        if !is_small(arity) {
            return Err(bad_arg());
        }
        // `signed_val` < 0 is rejected by the C; so is anything the wire could not
        // hold as a non-negative small.
        let Ok(arity) = usize::try_from(signed_value(arity)) else {
            return Err(bad_arg());
        };
        // The C truncates the 64-bit `Sint` to the 32-bit `unsigned` parameter of
        // `erts_export_get_or_make_stub` (`export.h:137`) and keeps the result in
        // `ErtsCodeMFA.arity`, a `Uint` (`code_ix.h:89-97`), so only the low 32 bits
        // survive (`external.c:5028-5038`); the encoder reads them back
        // (`external.c:4000`/`:5912`). Verified against erts-17.1: `2^32` ⇒ 0,
        // `2^32 + 7` ⇒ 7, `3 * 2^31` ⇒ `0x8000_0000`, `2^32 - 1` ⇒ `0xFFFF_FFFF`.
        let arity = arity & 0xFFFF_FFFF;
        crate::term::make_fun_external(heap, arity, atom_val(module), atom_val(function))
    }

    /// Decodes `NEW_FUN_EXT` (`112`):
    /// `[size:4 BE][arity:1][uniq:16][index:4 BE][num_free:4 BE][module atom]
    /// [old_index term][old_uniq term][creator pid term][env…]`
    /// (`external.c:5102-5176`).
    ///
    /// * `size` is **read and ignored**: since OTP 23 the C writes a total size that
    ///   may be wrong (`external.c:5115`), so it is not validated.
    /// * `old_index`/`old_uniq` must decode to **smalls** (`external.c:5139-5151`);
    ///   their value is stored as the fun's `u32` descriptor fields.
    /// * `num_free > MAX_ARG` (255) cannot fit the header's 8-bit `num_free` field
    ///   and is `BadArg`, never a corrupt header.
    /// * the creator pid is parsed straight off the wire (see
    ///   [`Decoder::decode_creator_pid`]) so no throw-away external-pid block is
    ///   left on `heap`.
    ///
    /// The `num_free` environment terms are decoded **in stream order**: stream
    /// element `k` is `env[k]` (C `external.c:5171-5174` links `env[0]` first; the
    /// C encoder emits `env[0]` first, `external.c:3991-3993`).
    pub(super) fn decode_new_fun(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
    ) -> Result<Eterm> {
        let _total_size = self.reader.u32be()?;
        let arity = usize::from(self.reader.u8()?);
        let mut uniq = [0u8; 16];
        uniq.copy_from_slice(self.reader.bytes(16)?);
        let index = self.reader.u32be()?;
        let num_free = self.reader.u32be()? as usize;
        if num_free > MAX_ARG {
            return Err(bad_arg());
        }
        let module = self.decode_node_atom(heap, atoms)?;
        let old_index = self.decode_small_u32(heap, atoms)?;
        let old_uniq = self.decode_small_u32(heap, atoms)?;
        let creator = self.decode_creator_pid(heap, atoms)?;
        // Wire order: the first environment term is `env[0]`.
        let mut env = Vec::new();
        for _ in 0..num_free {
            env.push(self.decode(heap, atoms)?);
        }
        crate::term::make_fun_imported(
            heap,
            arity,
            atom_val(module),
            &uniq,
            index,
            old_index,
            old_uniq,
            creator,
            &env,
        )
    }

    /// Decodes the next term and requires it to be a small, returning its value as
    /// the `u32` a fun's `old_index`/`old_uniq` descriptor field holds.
    ///
    /// The C reads those two `NEW_FUN_EXT` fields with `is_small(temp)` followed by
    /// `unsigned_val` (`external.c:5136-5151`); a non-small is `BadArg`.
    fn decode_small_u32(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<u32> {
        let term = self.decode(heap, atoms)?;
        if !is_small(term) {
            return Err(bad_arg());
        }
        Ok(signed_value(term) as u32)
    }

    /// Decodes a `NEW_FUN_EXT` creator pid **without** materializing a throw-away
    /// external-pid block on `heap`.
    ///
    /// The C decodes a term and requires `is_pid(temp)` before discarding it
    /// (`external.c:5153-5160`), so *every* pid form the generic walker accepts is
    /// valid here: `PID_EXT` (`103`), `NEW_PID_EXT` (`88`) and — because `[local]`
    /// makes the C substitute the internal `NIL` sysname for an internal pid
    /// (`ERTS_DIST_EXT_INTERNAL_NC`, `external.c:2978`) — a `LOCAL_EXT` (`121`)
    /// wrapper around either. Reading their fields directly keeps the node atom's
    /// interning in wire order and leaves the caller's heap free of the dead block.
    ///
    /// Because the C routes the creator through the same `dec_pid` the plain
    /// `Tag::Pid` arm uses, a *local* creator is subject to the same
    /// number/serial bound (D6) and a `LOCAL_EXT` hash is verified (D4) whenever
    /// the caller installed the identity ([`Decoder::with_local_node`] /
    /// [`Decoder::with_local_node_hash`]); both are inert otherwise.
    ///
    /// Any other tag is `BadArg`, exactly as the C's `is_pid` test rejects it.
    fn decode_creator_pid(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<FunPid> {
        let tag = Tag::from_byte(self.reader.u8()?).ok_or_else(bad_arg)?;
        match tag {
            Tag::Pid => {
                let node = self.decode_node_atom(heap, atoms)?;
                // The `[local]` internal-node marker names no sysname, and this inlined
                // creator descriptor stores a node **atom** (`FunPid::node`) — there is
                // no faithful value to store, so report it instead of keeping the `[]`
                // sentinel as the node (see [`Decoder::decode_node_atom`]).
                if self.is_internal_node_marker(node) {
                    return Err(unsupported(Tag::NewFun));
                }
                let number = self.reader.u32be()?;
                let serial = self.reader.u32be()?;
                let creation = u32::from(self.reader.u8()?);
                // The C routes the creator pid through the same `dec_pid` the plain
                // `Tag::Pid` arm uses (`external.c:5153-5160`), so its 8-bit creation
                // must be *tiny* (`is_tiny_creation`, `external.c:71-72`) — a value
                // above 3 is `badarg`, exactly as in the top-level `PID_EXT` arm.
                if creation > 3 {
                    return Err(bad_arg());
                }
                self.check_local_pid(node, creation, number, serial)?;
                Ok(FunPid {
                    node: atom_val(node),
                    number,
                    serial,
                    creation,
                })
            }
            Tag::NewPid => {
                let node = self.decode_node_atom(heap, atoms)?;
                // See the `Tag::Pid` arm: the `[local]` marker has no node atom to store.
                if self.is_internal_node_marker(node) {
                    return Err(unsupported(Tag::NewFun));
                }
                let number = self.reader.u32be()?;
                let serial = self.reader.u32be()?;
                let creation = self.reader.u32be()?;
                self.check_local_pid(node, creation, number, serial)?;
                Ok(FunPid {
                    node: atom_val(node),
                    number,
                    serial,
                    creation,
                })
            }
            // A `LOCAL_EXT`-wrapped pid: `dec_term`'s `case LOCAL_EXT` consumes the
            // 4-byte node hash and re-enters with the internal-node flag
            // (`external.c:5250-5254`), so the wrapped pid is the creator — and its
            // node field may be the `NIL_EXT` internal-node marker.
            Tag::Local => {
                let node_hash = self.reader.u32be()?;
                self.check_local_node_hash(node_hash)?;
                self.internal_node = true;
                self.decode_creator_pid(heap, atoms)
            }
            _ => Err(bad_arg()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Fun / export decode tests: the two wire shapes, the field rules (arity,
    //! `old_index`/`old_uniq`, `num_free`, creator pid) and the malformed cases.
    //!
    //! Tests are allowed to `unwrap` (a panic there is a test failure); the
    //! production bodies above never do.

    use super::*;
    use beam_term::prelude::make_small;

    use crate::decode::tests::{assert_bad_arg, decode};
    use crate::encode::encode_term;
    use crate::tag::{EXPORT_EXT, NEW_FUN_EXT};
    use crate::term::{
        fun_arity_term, fun_creator_pid, fun_env, fun_function, fun_index, fun_kind_term,
        fun_module, fun_num_free_term, fun_old_index, fun_old_uniq, fun_true_arity_term,
        fun_uniq_bytes, FunKind,
    };

    /// `EXPORT_EXT`: `[module atom][function atom][arity term]`.
    #[test]
    fn export_ext_decodes_to_an_external_fun() {
        // `fun lists:reverse/1` shape: module `lists`, function `reverse`, arity 1.
        let mut image = vec![131, EXPORT_EXT];
        image.extend_from_slice(&[119, 5, b'l', b'i', b's', b't', b's']);
        image.extend_from_slice(&[119, 7, b'r', b'e', b'v', b'e', b'r', b's', b'e']);
        image.push(97);
        image.push(1);
        let (_h, atoms, term) = decode(&image);
        assert_eq!(fun_kind_term(term).unwrap(), FunKind::External);
        assert_eq!(fun_arity_term(term).unwrap(), 1);
        assert_eq!(fun_num_free_term(term).unwrap(), 0);
        assert_eq!(
            atoms.name(fun_module(term).unwrap()),
            Some("lists"),
            "the module atom is interned in wire order"
        );
        assert_eq!(atoms.name(fun_function(term).unwrap()), Some("reverse"));
        assert_eq!(fun_env(term).unwrap(), &[] as &[Eterm]);

        // A non-small arity is malformed, as is a negative small.
        let mut bad = vec![131, EXPORT_EXT];
        bad.extend_from_slice(&[119, 1, b'm']);
        bad.extend_from_slice(&[119, 1, b'f']);
        bad.push(104); // SMALL_TUPLE_EXT — not a small
        bad.push(0);
        assert_bad_arg(&bad);

        let mut negative = vec![131, EXPORT_EXT];
        negative.extend_from_slice(&[119, 1, b'm']);
        negative.extend_from_slice(&[119, 1, b'f']);
        negative.extend_from_slice(&[98, 255, 255, 255, 255]); // INTEGER_EXT -1
        assert_bad_arg(&negative);

        // A module/function field that is not an atom is malformed.
        assert_bad_arg(&[131, EXPORT_EXT, 97, 1, 119, 1, b'f', 97, 0]);
        // An arity past the 8-bit header field decodes faithfully: `fun_arity_term`
        // reports the header mask (`arity & 0xFF`) while `fun_true_arity_term` keeps
        // the true value, exactly as the oracle splits `fun_info(F, arity)` from
        // `term_to_binary(F)`. The re-encode uses the true arity, so the image is a
        // fixed point (the width tag follows the same rule the C encoder uses:
        // `SMALL_INTEGER_EXT` below 256, else `INTEGER_EXT`).
        for arity in [255u32, 256, 257, 511, 512, 65_535, 65_536, 0x00ff_ffff] {
            let mut image = vec![131, EXPORT_EXT];
            image.extend_from_slice(&[119, 1, b'm']);
            image.extend_from_slice(&[119, 1, b'f']);
            if arity < 256 {
                image.extend_from_slice(&[97, arity as u8]); // SMALL_INTEGER_EXT
            } else {
                image.push(98); // INTEGER_EXT
                image.extend_from_slice(&arity.to_be_bytes());
            }
            let (_h, atoms, term) = decode(&image);
            assert_eq!(fun_arity_term(term).unwrap(), (arity & 0xFF) as usize);
            assert_eq!(fun_true_arity_term(term).unwrap(), arity as usize);
            assert_eq!(
                encode_term(term, &atoms).unwrap(),
                image,
                "EXPORT_EXT arity {arity} re-encodes byte-exactly"
            );
        }
    }

    /// `NEW_FUN_EXT`: the full six-field header, the creator pid and the env.
    #[test]
    fn new_fun_ext_decodes_to_an_imported_fun() {
        const NODE: [u8; 4] = [119, 2, b'n', b'1']; // the UTF-8 atom "n1"
        let mut uniq = [0u8; 16];
        for (i, byte) in uniq.iter_mut().enumerate() {
            *byte = i as u8 + 1;
        }

        let mut image = vec![131, NEW_FUN_EXT];
        image.extend_from_slice(&0u32.to_be_bytes()); // size — read, ignored
        image.push(2); // arity
        image.extend_from_slice(&uniq);
        image.extend_from_slice(&0x0102_0304u32.to_be_bytes()); // index
        image.extend_from_slice(&2u32.to_be_bytes()); // num_free
        image.extend_from_slice(&[119, 6, b'm', b'o', b'd', b'u', b'l', b'e']); // module
        image.extend_from_slice(&[97, 7]); // old_index 7
        image.extend_from_slice(&[97, 3]); // old_uniq 3
        // Creator pid as `NEW_PID_EXT` (88) — the form the C encoder emits.
        image.push(88);
        image.extend_from_slice(&NODE);
        image.extend_from_slice(&0xAAAA_BBBBu32.to_be_bytes());
        image.extend_from_slice(&0xCCCC_DDDDu32.to_be_bytes());
        image.extend_from_slice(&0x1122_3344u32.to_be_bytes());
        // Environment, in wire order: env[0] = 11, env[1] = 22.
        image.extend_from_slice(&[97, 11, 97, 22]);

        let (_h, atoms, term) = decode(&image);
        assert_eq!(fun_kind_term(term).unwrap(), FunKind::Imported);
        assert_eq!(fun_arity_term(term).unwrap(), 2);
        assert_eq!(fun_num_free_term(term).unwrap(), 2);
        assert_eq!(atoms.name(fun_module(term).unwrap()), Some("module"));
        assert_eq!(fun_uniq_bytes(term).unwrap(), uniq);
        assert_eq!(fun_index(term).unwrap(), 0x0102_0304);
        assert_eq!(fun_old_index(term).unwrap(), 7);
        assert_eq!(fun_old_uniq(term).unwrap(), 3);
        let creator = fun_creator_pid(term).unwrap();
        assert_eq!(atoms.name(creator.node), Some("n1"));
        assert_eq!((creator.number, creator.serial, creator.creation), (
            0xAAAA_BBBB,
            0xCCCC_DDDD,
            0x1122_3344
        ));
        // The stream order is the env order: element 0 is `env[0]`.
        assert_eq!(fun_env(term).unwrap(), &[make_small(11), make_small(22)]);

        // The `size` field is ignored even when it lies; only the field widths matter.
        let mut lying = image.clone();
        lying[2..6].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        let (_h, _a, term) = decode(&lying);
        assert_eq!(fun_arity_term(term).unwrap(), 2);

        // A legacy `PID_EXT` (103) creator is accepted (8-bit creation).
        let mut pid_legacy = vec![131, NEW_FUN_EXT];
        pid_legacy.extend_from_slice(&0u32.to_be_bytes());
        pid_legacy.push(0);
        pid_legacy.extend_from_slice(&uniq);
        pid_legacy.extend_from_slice(&1u32.to_be_bytes());
        pid_legacy.extend_from_slice(&0u32.to_be_bytes());
        pid_legacy.extend_from_slice(&[119, 1, b'm']);
        pid_legacy.extend_from_slice(&[97, 1, 97, 2]);
        pid_legacy.push(103);
        pid_legacy.extend_from_slice(&NODE);
        pid_legacy.extend_from_slice(&1u32.to_be_bytes());
        pid_legacy.extend_from_slice(&2u32.to_be_bytes());
        pid_legacy.push(3); // 8-bit creation
        let (_h, atoms, term) = decode(&pid_legacy);
        let creator = fun_creator_pid(term).unwrap();
        assert_eq!(atoms.name(creator.node), Some("n1"));
        assert_eq!((creator.number, creator.serial, creator.creation), (1, 2, 3));
        assert_eq!(fun_num_free_term(term).unwrap(), 0);
    }

    #[test]
    fn malformed_new_fun_ext_is_rejected() {
        // A `num_free` past the 8-bit header field.
        let mut too_free = vec![131, NEW_FUN_EXT];
        too_free.extend_from_slice(&0u32.to_be_bytes());
        too_free.push(0);
        too_free.extend_from_slice(&[0u8; 16]);
        too_free.extend_from_slice(&0u32.to_be_bytes());
        too_free.extend_from_slice(&256u32.to_be_bytes()); // num_free = 256
        assert_bad_arg(&too_free);

        // A creator field that is not a pid.
        let mut not_pid = vec![131, NEW_FUN_EXT];
        not_pid.extend_from_slice(&0u32.to_be_bytes());
        not_pid.push(0);
        not_pid.extend_from_slice(&[0u8; 16]);
        not_pid.extend_from_slice(&0u32.to_be_bytes());
        not_pid.extend_from_slice(&0u32.to_be_bytes());
        not_pid.extend_from_slice(&[119, 1, b'm']);
        not_pid.extend_from_slice(&[97, 1, 97, 2, 97, 3]); // creator is a small
        assert_bad_arg(&not_pid);

        // `old_index`/`old_uniq` must be smalls.
        let mut not_small = vec![131, NEW_FUN_EXT];
        not_small.extend_from_slice(&0u32.to_be_bytes());
        not_small.push(0);
        not_small.extend_from_slice(&[0u8; 16]);
        not_small.extend_from_slice(&0u32.to_be_bytes());
        not_small.extend_from_slice(&0u32.to_be_bytes());
        not_small.extend_from_slice(&[119, 1, b'm']);
        not_small.extend_from_slice(&[104, 0]); // SMALL_TUPLE_EXT
        not_small.extend_from_slice(&[97, 1]);
        not_small.push(88);
        not_small.extend_from_slice(&[119, 2, b'n', b'1']);
        not_small.extend_from_slice(&[0u8; 12]);
        assert_bad_arg(&not_small);
    }
}
