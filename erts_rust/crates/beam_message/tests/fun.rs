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

//! Integration tests for **fun (closure) blocks** crossing a copy boundary.
//!
//! The copier sizes and walks a fun from the **frozen** `beam_term::fun` layout — the M1 inlined
//! descriptor, where `env[i]` lives at `1 + desc_words(kind) + i` and the block is
//! `1 + desc_words(kind) + num_free` words — and never from the legacy C `ERL_FUN_SIZE = 2`
//! (`ErlFunThing`'s `[thing_word, entry, env[]]`). These tests pin that: a closure pushed through
//! [`copy_struct_simple`] must occupy exactly the block its `beam_term` layout prescribes (the
//! legacy math under-sizes a `Local` fun by one word and would visit the descriptor's `index` word
//! as if it were `env[0]`), compare `=:=` to the original, encode to identical `NEW_FUN_EXT` bytes
//! and still resolve to a callable lambda.
//!
//! Evidence covered (crate `CONTEXT.md` `Test Strategy`):
//!
//! 1. a zero-free-variable (`make_fun`) closure copies exactly and compares equal,
//! 2. a `make_fun3` closure with several free variables (heap terms included) sizes/walks its
//!    whole environment,
//! 3. `term_to_binary` of the original and of the copy agree byte-for-byte,
//! 4. the copied closure can still be **called** (its identity resolves in the lambda table and its
//!    environment is intact),
//! 5. `env[0]` sits at `1 + desc_words(kind)`, fixed up into the destination heap — not at the
//!    descriptor's `index` word.

use std::collections::HashMap;

use beam_alloc::Heap;
use beam_codec::{Encoder, FunIdentity};
use beam_message::{copy_struct_simple, size_of_copy};
use beam_term::prelude::*;
use beam_term::{compare, signed_value, size_object};

/// Builds a `Local` (`make_fun3`) closure block on `heap`:
/// `[header][uniq][index][env[0..num_free]]` ([`beam_term::fun::write_fun_local`]).
fn make_local_fun(heap: &mut Heap, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    // SAFETY: `fun_words(Local, env.len())` is the block's exact size and `write_fun_local` writes
    // exactly those words; every `env` term is a live term (or an immediate).
    unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Local, env.len()))
            .expect("the fun block fits the source heap");
        write_fun_local(hp, arity, uniq, index, env).expect("a valid local-fun header")
    }
}

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first, synchronously.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits the source heap");
    }
    acc
}

/// The address of the object a non-immediate compound term points at.
fn addr_of(term: Eterm) -> usize {
    if is_list(term) {
        list_val(term) as usize
    } else {
        boxed_val(term) as usize
    }
}

/// Whether `addr` lies inside `heap`'s region (`[base, hend)`).
fn heap_contains(heap: &Heap, addr: usize) -> bool {
    let base = heap.hp() as usize - heap.used() * std::mem::size_of::<Eterm>();
    addr >= base && addr < heap.hend() as usize
}

/// The body of a closure in the test lambda table: `(environment, arguments) -> result`.
type LambdaBody = fn(&[Eterm], &[Eterm]) -> Eterm;

/// Applies a `Local` closure the way `beam_engine`'s `apply` does: resolve the closure's
/// `(uniq, index)` identity in the running module's lambda table, then run the body with the
/// closure's environment and the call arguments.
///
/// Using this on both the original and its copy is what makes "the copied closure can still be
/// called" a real assertion: the identity (`uniq`/`index`) **and** the environment (including
/// boxed free variables) must both have survived the copy. (`beam_message` cannot depend on
/// `beam_engine` — the wave DAG runs the other way — so the lambda table stands in for the
/// engine's.)
fn apply(closure: Eterm, table: &HashMap<(u64, u32), LambdaBody>, args: &[Eterm]) -> Eterm {
    // SAFETY: `closure` is a live local fun owned by a heap this test keeps alive, so its
    // descriptor words and its environment are live.
    let (uniq, index, env) = unsafe {
        (
            fun_uniq(closure).expect("a local fun carries its `uniq`"),
            fun_index(closure).expect("a local fun carries its `index`"),
            fun_env(closure).expect("a local fun carries its environment"),
        )
    };
    let body = table
        .get(&(uniq, index))
        .expect("the closure's `(uniq, index)` identity resolves in the lambda table");
    // SAFETY: word 0 of a live fun is its header word.
    assert_eq!(
        fun_arity(unsafe { *boxed_val(closure) }),
        args.len(),
        "the closure is applied at its own arity"
    );
    body(env, args)
}

/// `fun(X) -> Env0 + X end`.
fn add_env(env: &[Eterm], args: &[Eterm]) -> Eterm {
    make_small(signed_value(env[0]) + signed_value(args[0]))
}

/// `fun() -> element(1, Env0) end` — reads a **boxed** free variable, so it only produces the
/// closure's answer if the environment was copied/fixed up correctly.
fn second_of_env_tuple(env: &[Eterm], _args: &[Eterm]) -> Eterm {
    // SAFETY: the test only ever installs this body on closures whose `env[0]` is the 2-tuple it
    // built, so element 1 is in bounds.
    unsafe { tuple_element(env[0], 1) }
}

#[test]
fn zero_free_var_closure_copies_to_the_frozen_block_and_compares_equal() {
    const UNIQ: u64 = 0xDEAD_BEEF_CAFE_F00D;
    const INDEX: u32 = 0x0102_0304;
    let atoms = AtomTable::new();

    let mut src = Heap::new(32).expect("source heap");
    // `erlang:make_fun(M, F, A)` has no environment: `1 (header) + 2 (uniq, index)` words.
    let original = make_local_fun(&mut src, 1, UNIQ, INDEX, &[]);
    assert_eq!(fun_block_words(original).expect("a live local fun"), 3);

    let mut dst = Heap::new(32).expect("destination heap");
    let copy = copy_struct_simple(original, &mut dst).expect("the copy succeeds");

    assert!(
        compare::eq(original, copy, &atoms),
        "the copy must be =:= the original"
    );
    // The frozen block. The legacy `ERL_FUN_SIZE + num_free` math would reserve only 2 words here
    // and silently drop the `index` descriptor word.
    assert_eq!(
        dst.used(),
        3,
        "a zero-free-variable closure occupies exactly 3 destination words"
    );
    assert_eq!(dst.used(), size_of_copy(original));
    assert_eq!(dst.used(), size_object(original));
    assert_ne!(
        boxed_val(original) as usize,
        boxed_val(copy) as usize,
        "a genuine copy"
    );

    // SAFETY: `copy` is a live local fun on `dst`.
    unsafe {
        assert_eq!(fun_uniq(copy).expect("uniq survives"), UNIQ);
        assert_eq!(fun_index(copy).expect("index survives"), INDEX);
        assert!(fun_env(copy).expect("env is empty").is_empty());
    }
}

#[test]
fn closure_with_several_free_vars_sizes_its_whole_env_and_copies_it() {
    let mut atoms = AtomTable::new();
    let tag = make_atom(atoms.intern("tag"));

    let mut src = Heap::new(64).expect("source heap");
    let tuple = src
        .tuple(&[make_small(1), make_small(2), tag])
        .expect("the tuple fits");
    let list = build_list(&mut src, &[make_small(3), make_small(4)]);
    let env = [make_small(7), tag, tuple, list, NIL];
    let original = make_local_fun(&mut src, 3, 42, 9, &env);

    let mut dst = Heap::new(64).expect("destination heap");
    let copy = copy_struct_simple(original, &mut dst).expect("the copy succeeds");

    // `1 + 2 (descriptor) + 5 (env)` plus the graphs the two boxed free variables reach: every
    // word is written exactly once, with no trailing/duplicated word.
    let expected = fun_words(FunKind::Local, env.len()) + size_object(tuple) + size_object(list);
    assert_eq!(
        dst.used(),
        expected,
        "the closure block plus its environment graphs, no more"
    );
    assert_eq!(dst.used(), size_of_copy(original));
    assert!(
        compare::eq(original, copy, &atoms),
        "the copy must be =:= the original"
    );

    // SAFETY: both terms are live local funs, of the source and the destination heap respectively.
    let (src_env, dst_env) = unsafe {
        (
            fun_env(original).expect("the env is live").to_vec(),
            fun_env(copy).expect("the env is live").to_vec(),
        )
    };
    assert_eq!(dst_env.len(), src_env.len(), "every free variable survives");
    for (i, (a, b)) in src_env.iter().zip(dst_env.iter()).enumerate() {
        assert!(compare::eq(*a, *b, &atoms), "env[{i}] survives the copy");
    }
    // The boxed free variables are genuine destination-heap copies, not shared source pointers.
    assert_ne!(addr_of(dst_env[2]), addr_of(src_env[2]));
    assert_ne!(addr_of(dst_env[3]), addr_of(src_env[3]));
    assert!(heap_contains(&dst, addr_of(dst_env[2])));
    assert!(heap_contains(&dst, addr_of(dst_env[3])));
}

#[test]
fn term_to_binary_of_the_original_and_the_copy_agree_byte_for_byte() {
    let mut atoms = AtomTable::new();
    let module = atoms.intern("m");
    let node = atoms.intern("n1");
    let tag = make_atom(atoms.intern("tag"));

    let mut src = Heap::new(64).expect("source heap");
    let tuple = src.tuple(&[make_small(1), tag]).expect("the tuple fits");
    let env = [make_small(11), tag, tuple];
    let original = make_local_fun(&mut src, 2, 0xABCD_1234, 0x0102_0304, &env);

    let mut dst = Heap::new(64).expect("destination heap");
    let copy = copy_struct_simple(original, &mut dst).expect("the copy succeeds");

    let mut uniq = [0u8; 16];
    for (i, byte) in uniq.iter_mut().enumerate() {
        *byte = i as u8 + 1;
    }
    let identity = FunIdentity::new(
        module,
        uniq,
        7,
        3,
        FunPid {
            node,
            number: 0xAAAA_BBBB,
            serial: 0xCCCC_DDDD,
            creation: 0x1122_3344,
        },
    );

    // A local fun carries no module/MD5, so the wire identity comes from the caller; both funs get
    // the same one, and the bytes must then be identical.
    let mut enc_original = Encoder::new(&atoms).with_fun_identity(identity);
    enc_original
        .encode(original)
        .expect("the local fun encodes with a supplied identity");
    let mut enc_copy = Encoder::new(&atoms).with_fun_identity(identity);
    enc_copy.encode(copy).expect("so does the copy");

    assert_eq!(
        enc_copy.as_bytes(),
        enc_original.as_bytes(),
        "identical `NEW_FUN_EXT` image"
    );
    assert_eq!(enc_original.as_bytes()[0], 131, "VERSION_MAGIC");
    assert_eq!(enc_original.as_bytes()[1], 112, "NEW_FUN_EXT");
    assert_ne!(
        enc_original.as_bytes(),
        &[131u8, 112][..],
        "the payload is non-empty"
    );
}

#[test]
fn copied_closure_can_still_be_called() {
    let mut table: HashMap<(u64, u32), LambdaBody> = HashMap::new();
    table.insert((7, 1), add_env);
    table.insert((9, 2), second_of_env_tuple);

    let mut src = Heap::new(64).expect("source heap");
    // `fun(X) -> 10 + X end` — closes over a small integer.
    let adder = make_local_fun(&mut src, 1, 7, 1, &[make_small(10)]);
    // `fun() -> element(1, {1, 2}) end` — closes over a heap tuple.
    let tuple = src
        .tuple(&[make_small(1), make_small(2)])
        .expect("the tuple fits");
    let picker = make_local_fun(&mut src, 0, 9, 2, &[tuple]);

    let mut dst = Heap::new(64).expect("destination heap");
    let adder_copy = copy_struct_simple(adder, &mut dst).expect("the adder copies");
    let picker_copy = copy_struct_simple(picker, &mut dst).expect("the picker copies");

    // Both the original and the copy resolve the same lambda identity and run to the same answer —
    // the copy is callable, with its environment intact.
    let add_args = [make_small(5)];
    assert_eq!(signed_value(apply(adder, &table, &add_args)), 15);
    assert_eq!(signed_value(apply(adder_copy, &table, &add_args)), 15);
    assert_eq!(signed_value(apply(adder, &table, &[make_small(-3)])), 7);
    assert_eq!(
        signed_value(apply(adder_copy, &table, &[make_small(-3)])),
        7
    );

    assert_eq!(signed_value(apply(picker, &table, &[])), 2);
    assert_eq!(signed_value(apply(picker_copy, &table, &[])), 2);
}

#[test]
fn env_offset_is_one_plus_desc_words_not_erl_fun_size() {
    const UNIQ: u64 = 0x0011_2233_4455_6677;
    const INDEX: u32 = 0x0010_0010; // low bits clear: only the *sizing* can catch a mix-up
    let mut atoms = AtomTable::new();
    let tag = make_atom(atoms.intern("tag"));

    let mut src = Heap::new(64).expect("source heap");
    let tuple = src.tuple(&[make_small(1), tag]).expect("the tuple fits");
    let original = make_local_fun(&mut src, 1, UNIQ, INDEX, &[tuple]);
    assert_eq!(
        fun_block_words(original).expect("a live local fun"),
        1 + 2 + 1
    );

    let mut dst = Heap::new(64).expect("destination heap");
    let copy = copy_struct_simple(original, &mut dst).expect("the copy succeeds");

    assert_eq!(
        dst.used(),
        fun_words(FunKind::Local, 1) + size_object(tuple),
        "the fun block (`1 + desc_words + num_free`) plus the copied free variable"
    );

    // The word at offset 2 is the raw `index` descriptor, and `env[0]` is at `1 + desc_words == 3`.
    // SAFETY: `copy` is a live local fun block of exactly `fun_words(Local, 1) == 4` words on `dst`.
    let block = boxed_val(copy);
    unsafe {
        assert_eq!(
            *block.add(2),
            Eterm::from_raw(INDEX as usize),
            "word 2 is the descriptor"
        );
        let env0 = *block.add(3);
        assert!(!is_immediate(env0), "word 3 is `env[0]`, a heap term");
        assert!(
            heap_contains(&dst, addr_of(env0)),
            "`env[0]` was fixed up into the destination"
        );
        assert!(
            is_arity_value(*boxed_val(env0)),
            "and it is the copied free-variable tuple"
        );
        assert_eq!(
            tuple_element(env0, 0),
            make_small(1),
            "the copied environment still holds its element"
        );
    }
}
