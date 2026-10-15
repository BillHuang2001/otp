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

//! The fun / export encoders of [`Encoder`]: `EXPORT_EXT` (`113`) and
//! `NEW_FUN_EXT` (`112`).
//!
//! [`Encoder::encode`]'s classification chain (in the parent [`crate::encode`]
//! module) probes the fun family with `fun_kind_term` **before** the other boxed
//! families — a fun header carries a distinct subtag, so the classification cannot
//! overlap — and dispatches here by kind. The encoders live in this child module as
//! a second `impl` block on the encoder, so they reach its private output buffer,
//! atom writer and integer-width helper directly.
//!
//! This module is safe Rust, like its parent.

use beam_common::Result;
use beam_term::prelude::{make_atom, Atom};
use beam_term::Eterm;

use crate::error;
use crate::tag::{EXPORT_EXT, NEW_FUN_EXT};
use crate::term::{
    fun_arity_term, fun_creator_pid, fun_env, fun_function, fun_index, fun_module,
    fun_num_free_term, fun_old_index, fun_old_uniq, fun_true_arity_term, fun_uniq_bytes, FunKind,
    FunPid,
};

use super::{Encoder, FunIdentity};

impl<'a> Encoder<'a> {
    /// Encodes a fun term by kind (`external.c:3966-4004`).
    ///
    /// `FunKind::External` becomes `EXPORT_EXT` (`113`) and `FunKind::Imported`
    /// becomes `NEW_FUN_EXT` (`112`).
    ///
    /// `FunKind::Local` — the `make_fun3` closure — has **no** self-contained wire
    /// image: its inlined descriptor carries no module, no 16-byte `uniq` and no
    /// creator pid. It is encoded as `NEW_FUN_EXT` only when the caller supplied
    /// that identity through [`Encoder::with_fun_identity`] (the runtime's module
    /// identity); otherwise it is reported as [`crate::error::unsupported_local_fun`]
    /// rather than mis-encoded.
    pub(super) fn encode_fun(&mut self, term: Eterm, kind: FunKind) -> Result<()> {
        match kind {
            FunKind::External => self.encode_fun_external(term),
            FunKind::Imported => self.encode_fun_imported(term),
            // `Option<FunIdentity>` is `Copy`, so the read releases `self` before the
            // `&mut self` call below.
            FunKind::Local => match self.fun_identity {
                Some(identity) => self.encode_fun_local(term, identity),
                None => Err(error::unsupported_local_fun()),
            },
        }
    }

    /// Encodes an `EXPORT_EXT` fun: `[113][module atom][function atom][arity small]`
    /// (`external.c:3997-4001`).
    ///
    /// The arity is the **true** arity (descriptor word 3), never the header's 8-bit
    /// mask: the C reads the full arity back out of its out-of-line `Export` table
    /// (`external.c:5038`) and re-emits it, so an arity above 255 must survive here
    /// too (the header field saturates while the term keeps the true value —
    /// [`crate::term::fun_true_arity_term`]).
    fn encode_fun_external(&mut self, term: Eterm) -> Result<()> {
        let module = fun_module(term)?;
        let function = fun_function(term)?;
        let arity = fun_true_arity_term(term)?;
        self.buf.u8(EXPORT_EXT);
        self.encode_atom(make_atom(module))?;
        self.encode_atom(make_atom(function))?;
        // The C writes `make_small(arity)`; `encode_small` picks the width tag.
        self.encode_small(arity as i64)?;
        Ok(())
    }

    /// Encodes a `NEW_FUN_EXT` fun:
    ///
    /// ```text
    /// [112][size:4 BE][arity:1][uniq:16][index:4 BE][num_free:4 BE]
    /// [module][old_index small][old_uniq small][creator pid: NEW_PID_EXT]
    /// [env[0] … env[num_free-1]]
    /// ```
    ///
    /// `size` is the number of bytes from the **first byte of the size field**
    /// through the end of the fun (`4 + payload_len`) — the C computes exactly
    /// `ep - size_p` where `size_p` is the size field's own address
    /// (`external.c:3363`). It is only known once the body is emitted, so a
    /// placeholder is written and [`Writer::patch_u32be`](crate::Writer::patch_u32be)
    /// patches it; that is the documented internal invariant of the patch.
    ///
    /// The environment is emitted `env[0]` first (`external.c:3991-3993` pushes
    /// `env[num_free-1]`…`env[0]` onto a LIFO work stack), the same order the
    /// decoder reads it (`external.c:5171-5174`), so `env[k]` is stream element `k`.
    fn encode_fun_imported(&mut self, term: Eterm) -> Result<()> {
        let arity = fun_arity_term(term)?;
        let num_free = fun_num_free_term(term)?;
        let module = fun_module(term)?;
        let uniq = fun_uniq_bytes(term)?;
        let index = fun_index(term)?;
        let old_index = fun_old_index(term)?;
        let old_uniq = fun_old_uniq(term)?;
        let creator = fun_creator_pid(term)?;
        let env = fun_env(term)?;

        self.encode_new_fun_ext(
            arity, num_free, module, &uniq, index, old_index, old_uniq, creator, env,
        )
    }

    /// Encodes a local (`make_fun3`) fun as `NEW_FUN_EXT` (`112`).
    ///
    /// The C encoder (`external.c:3966-3993`, the `is_local_fun(funp)` branch) treats
    /// a local fun exactly like an imported one: it writes the same `NEW_FUN_EXT`
    /// body, taking `arity`/`num_free`/`env` from the **term** and
    /// `module`/`uniq`/`index`/`old_index`/`old_uniq` from the fun's `ErlFunEntry`
    /// (the module's lambda table) with the creator pid fixed to
    /// `erts_init_process_id`. Only the term's own `index` differs in source
    /// (descriptor word 2 for a local fun vs. word 4 for an imported one —
    /// [`fun_index`] abstracts that away).
    ///
    /// The identity fields a local *term* does not carry come from the supplied
    /// [`FunIdentity`]; see [`Encoder::with_fun_identity`].
    fn encode_fun_local(&mut self, term: Eterm, identity: FunIdentity) -> Result<()> {
        let arity = fun_arity_term(term)?;
        let num_free = fun_num_free_term(term)?;
        let index = fun_index(term)?;
        let env = fun_env(term)?;

        self.encode_new_fun_ext(
            arity,
            num_free,
            identity.module,
            &identity.uniq,
            index,
            identity.old_index,
            identity.old_uniq,
            identity.creator,
            env,
        )
    }

    /// Emits the shared `NEW_FUN_EXT` body (used for both imported and, with a
    /// supplied identity, local funs — see [`Encoder::encode_fun_imported`] and
    /// [`Encoder::encode_fun_local`]).
    #[allow(clippy::too_many_arguments)]
    fn encode_new_fun_ext(
        &mut self,
        arity: usize,
        num_free: usize,
        module: Atom,
        uniq: &[u8; 16],
        index: u32,
        old_index: u32,
        old_uniq: u32,
        creator: FunPid,
        env: &[Eterm],
    ) -> Result<()> {
        self.buf.u8(NEW_FUN_EXT);
        // The size field's own position: `size` counts from here (`ep - size_p`).
        let size_pos = self.buf.len();
        self.buf.u32be(0); // placeholder, patched once the body is written
        // `arity`/`num_free` both fit their 8-bit header fields (the layout enforced
        // that when the term was built), so these casts cannot truncate.
        self.buf.u8(arity as u8);
        self.buf.bytes(uniq);
        self.buf.u32be(index);
        self.buf.u32be(num_free as u32);
        self.encode_atom(make_atom(module))?;
        self.encode_small(i64::from(old_index))?;
        self.encode_small(i64::from(old_uniq))?;
        // The creator pid is always the modern `NEW_PID_EXT` (88) — the form the C
        // `enc_pid` emits here (`external.c:3989`).
        self.encode_pid(make_atom(creator.node), creator.number, creator.serial, creator.creation)?;
        for &free in env {
            self.encode(free)?;
        }
        let size = u32::try_from(self.buf.len() - size_pos).map_err(|_| error::system_limit())?;
        self.buf.patch_u32be(size_pos, size)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Byte-exact fun encoder tests: `EXPORT_EXT`/`NEW_FUN_EXT` are asserted against
    //! the exact wire bytes, including the C `size` rule and the local-fun rejection.

    use beam_term::prelude::make_small;
    use beam_term::AtomTable;

    use crate::encode::tests::new_heap;
    use crate::encode::{encode_term, encode_term_body, Encoder, FunIdentity};
    use crate::error::{is_unsupported, is_unsupported_local_fun};
    use crate::tag::NEW_FUN_EXT;
    use crate::term::{make_fun_external, make_fun_imported, make_fun_local, FunPid, MAX_ARG};

    #[test]
    fn external_fun_encodes_as_export_ext() {
        let mut atoms = AtomTable::new();
        let module = atoms.intern("lists");
        let function = atoms.intern("reverse");
        let mut heap = new_heap(64);
        let fun = make_fun_external(&mut heap, 1, module, function).unwrap();

        // `term_to_binary(fun lists:reverse/1)`:
        // `[113][SMALL_ATOM_UTF8 "lists"][SMALL_ATOM_UTF8 "reverse"][SMALL_INTEGER 1]`.
        let mut expected = vec![131u8, 113];
        expected.extend_from_slice(&[119, 5, b'l', b'i', b's', b't', b's']);
        expected.extend_from_slice(&[119, 7, b'r', b'e', b'v', b'e', b'r', b's', b'e']);
        expected.extend_from_slice(&[97, 1]);
        assert_eq!(encode_term(fun, &atoms).unwrap(), expected);

        // Arity 0 is the `SMALL_INTEGER_EXT` zero; the byte form is unchanged.
        let zero = make_fun_external(&mut heap, 0, module, function).unwrap();
        let bytes = encode_term_body(zero, &atoms).unwrap();
        assert_eq!(bytes[0], 113);
        assert_eq!(*bytes.last().unwrap(), 0);
        assert_eq!(bytes.len(), 1 + (2 + 5) + (2 + 7) + 2);

        // Arity 255 is still a single `SMALL_INTEGER_EXT`.
        let max = make_fun_external(&mut heap, MAX_ARG, module, function).unwrap();
        let bytes = encode_term_body(max, &atoms).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[97, 255]);

        // An arity above the 8-bit header field encodes the **true** arity: the C
        // reads it out of its `Export` table (`external.c:5038`), so 256 is a 4-byte
        // `INTEGER_EXT` `0,0,1,0` — not the header's masked `[97, 0]`.
        let over = make_fun_external(&mut heap, MAX_ARG + 1, module, function).unwrap();
        let bytes = encode_term_body(over, &atoms).unwrap();
        assert_eq!(&bytes[bytes.len() - 5..], &[98, 0, 0, 1, 0]);

        // 511 masks to 255 in the header, but the true 511 is what goes on the wire.
        let big = make_fun_external(&mut heap, 511, module, function).unwrap();
        let bytes = encode_term_body(big, &atoms).unwrap();
        assert_eq!(&bytes[bytes.len() - 5..], &[98, 0, 0, 1, 255]);
    }

    #[test]
    fn imported_fun_encodes_as_new_fun_ext_with_the_c_size_rule() {
        let mut atoms = AtomTable::new();
        let module = atoms.intern("m");
        let node = atoms.intern("n1");
        let mut uniq = [0u8; 16];
        for (i, byte) in uniq.iter_mut().enumerate() {
            *byte = i as u8 + 1; // 1..=16
        }
        let creator = FunPid {
            node,
            number: 0xAAAA_BBBB,
            serial: 0xCCCC_DDDD,
            creation: 0x1122_3344,
        };
        let env = [make_small(11), make_small(22)];
        let mut heap = new_heap(64);
        let fun =
            make_fun_imported(&mut heap, 2, module, &uniq, 0x0102_0304, 7, 3, creator, &env)
                .unwrap();

        let mut payload = Vec::new();
        payload.push(2u8); // arity
        payload.extend_from_slice(&uniq);
        payload.extend_from_slice(&0x0102_0304u32.to_be_bytes()); // index
        payload.extend_from_slice(&2u32.to_be_bytes()); // num_free
        payload.extend_from_slice(&[119, 1, b'm']); // module atom
        payload.extend_from_slice(&[97, 7]); // old_index small
        payload.extend_from_slice(&[97, 3]); // old_uniq small
        payload.push(88); // NEW_PID_EXT
        payload.extend_from_slice(&[119, 2, b'n', b'1']);
        payload.extend_from_slice(&0xAAAA_BBBBu32.to_be_bytes());
        payload.extend_from_slice(&0xCCCC_DDDDu32.to_be_bytes());
        payload.extend_from_slice(&0x1122_3344u32.to_be_bytes());
        // The environment is emitted `env[0]` first.
        payload.extend_from_slice(&[97, 11, 97, 22]);

        // `size` counts from the first byte of the size field through the end of the
        // fun — `4 + payload_len` (C `ep - size_p`, `external.c:3363`).
        let size = 4 + payload.len();
        let mut expected = vec![112u8];
        expected.extend_from_slice(&(size as u32).to_be_bytes());
        expected.extend_from_slice(&payload);

        assert_eq!(encode_term_body(fun, &atoms).unwrap(), expected);
        // The size field excludes the `112` tag itself.
        assert_eq!(expected.len(), 1 + size);
    }

    #[test]
    fn imported_fun_with_no_free_variables_round_trips_its_fields() {
        let mut atoms = AtomTable::new();
        let module = atoms.intern("m");
        let creator = FunPid {
            node: atoms.intern("n1"),
            number: 1,
            serial: 2,
            creation: 3,
        };
        let mut heap = new_heap(64);
        let fun =
            make_fun_imported(&mut heap, 3, module, &[0x5Au8; 16], 9, 8, 7, creator, &[]).unwrap();
        let bytes = encode_term_body(fun, &atoms).unwrap();

        // `112 [size 4]` then `arity` immediately follows.
        assert_eq!(bytes[0], 112);
        assert_eq!(bytes[5], 3); // arity
        assert_eq!(bytes[6..22], [0x5Au8; 16]); // uniq
        assert_eq!(&bytes[22..26], &9u32.to_be_bytes()); // index
        assert_eq!(&bytes[26..30], &0u32.to_be_bytes()); // num_free
        // The encoded body is exactly `size` bytes.
        let size = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        assert_eq!(bytes.len(), 1 + size);
    }

    #[test]
    fn local_fun_has_no_wire_form() {
        let atoms = AtomTable::new();
        let mut heap = new_heap(64);
        let local = make_fun_local(&mut heap, 0, 0xDEAD_BEEF, 5, &[]).unwrap();
        let err = encode_term(local, &atoms).unwrap_err();
        assert!(
            is_unsupported_local_fun(&err),
            "a local fun must report the precise local-fun reason, got {err:?}"
        );
        // The general "unsupported tag" signal is deliberately *not* reused.
        assert!(!is_unsupported(&err));
    }

    #[test]
    fn local_fun_encodes_as_new_fun_ext_with_supplied_identity() {
        let mut atoms = AtomTable::new();
        let module = atoms.intern("m");
        let node = atoms.intern("n1");
        let mut uniq = [0u8; 16];
        for (i, byte) in uniq.iter_mut().enumerate() {
            *byte = i as u8 + 1; // 1..=16
        }
        let creator = FunPid {
            node,
            number: 0xAAAA_BBBB,
            serial: 0xCCCC_DDDD,
            creation: 0x1122_3344,
        };
        let env = [make_small(11), make_small(22)];
        // The term's own descriptor `uniq` is engine-internal and never on the wire,
        // so an arbitrary value must not affect the bytes.
        let mut heap = new_heap(64);
        let local = make_fun_local(&mut heap, 2, 0xDEAD_BEEF, 0x0102_0304, &env).unwrap();

        let identity = FunIdentity::new(module, uniq, 7, 3, creator);
        let mut encoder = Encoder::new(&atoms).with_fun_identity(identity);
        encoder.encode(local).unwrap();

        let mut payload = Vec::new();
        payload.push(2u8); // arity
        payload.extend_from_slice(&uniq);
        payload.extend_from_slice(&0x0102_0304u32.to_be_bytes()); // index (term descriptor)
        payload.extend_from_slice(&2u32.to_be_bytes()); // num_free
        payload.extend_from_slice(&[119, 1, b'm']); // module atom
        payload.extend_from_slice(&[97, 7]); // old_index small
        payload.extend_from_slice(&[97, 3]); // old_uniq small
        payload.push(88); // NEW_PID_EXT
        payload.extend_from_slice(&[119, 2, b'n', b'1']);
        payload.extend_from_slice(&0xAAAA_BBBBu32.to_be_bytes());
        payload.extend_from_slice(&0xCCCC_DDDDu32.to_be_bytes());
        payload.extend_from_slice(&0x1122_3344u32.to_be_bytes());
        payload.extend_from_slice(&[97, 11, 97, 22]); // env[0] first

        let size = 4 + payload.len();
        let mut expected = vec![131u8, 112];
        expected.extend_from_slice(&(size as u32).to_be_bytes());
        expected.extend_from_slice(&payload);

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    #[test]
    fn the_function_name_is_never_encoded() {
        let mut atoms = AtomTable::new();
        let module = atoms.intern("m");
        let node = atoms.intern("n1");
        let mut uniq = [0u8; 16];
        for (i, byte) in uniq.iter_mut().enumerate() {
            *byte = i as u8 + 1; // 1..=16
        }
        let creator = FunPid {
            node,
            number: 0xAAAA_BBBB,
            serial: 0xCCCC_DDDD,
            creation: 0x1122_3344,
        };
        let env = [make_small(11), make_small(22)];
        let mut heap = new_heap(64);
        let local = make_fun_local(&mut heap, 2, 0xDEAD_BEEF, 0x0102_0304, &env).unwrap();

        let unnamed = FunIdentity::new(module, uniq, 7, 3, creator);
        assert_eq!(unnamed.function, None);
        let mut encoder = Encoder::new(&atoms).with_fun_identity(unnamed);
        encoder.encode(local).unwrap();
        let without = encoder.as_bytes().to_vec();

        // The lambda's `FunT` function-name atom is introspection-only metadata
        // (`erlang:fun_info/2`'s `name` item): `NEW_FUN_EXT` has no function-name
        // field, so attaching it must leave the wire image byte-for-byte identical.
        let lambda = atoms.intern("-mk/1-fun-0-");
        let named = FunIdentity::named(module, uniq, 7, 3, creator, lambda);
        assert_eq!(named.function, Some(lambda));
        let mut encoder = Encoder::new(&atoms).with_fun_identity(named);
        encoder.encode(local).unwrap();
        let with = encoder.as_bytes().to_vec();

        assert_eq!(without, with);
        assert_eq!(with[1], NEW_FUN_EXT);
    }
}
