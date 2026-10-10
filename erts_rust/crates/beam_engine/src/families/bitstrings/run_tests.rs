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

//! End-to-end integration tests for the bitstring family's two **readable-subject**
//! paths, driven through the real public execution entries
//! ([`Machine::run_label`] / [`Machine::run_export`]) over synthetic
//! `beam_loader::Module`s and a [`MinimalContext`] — never by calling a family
//! helper directly.
//!
//! Two capabilities the family used to refuse are pinned here from the *outside*:
//!
//! 1. **A refc-subject window that does not start on a byte boundary.**
//!    [`raw::heap_bits`] recovers `(base, size, off)` for an off-heap refc binary
//!    from the frozen `ErlSubBits`/`BinRef` pair, so a `bs_start_match3` +
//!    `bs_get_tail` (or `bs_get_integer2`) sequence over such a subject reads the
//!    window's bits exactly. The tests below build the refc term in the harness with
//!    the frozen [`beam_term::refc::write_refc_binary`] layout, at a window start of
//!    one bit, and assert the returned bitstring bit-for-bit.
//! 2. **An `append`/`private_append` segment in a non-first position.**
//!    `bs_create_bin`'s segment walk concatenates the source bitstring at the point
//!    the appending segment appears, so `<<7:8, Src/bits>>` builds through a
//!    synthetic segment list `[integer, …] [append, …]`.
//!
//! A third suite is a **live `erlc`/`erl` oracle cross-check**: an inline Erlang
//! source string is compiled at test time with the system `erlc`, loaded with
//! [`beam_loader::load_file`], run with [`Machine::run_export`], and compared with
//! the real system `erl` on the *same* subject bytes. The whole suite **skips
//! loudly** (never a silent pass) when `erlc`/`erl` are not on `PATH`.
//!
//! `unsafe` is confined to the frozen-layout readers here (the same
//! `bitstring_parts`/`write_refc_binary` accessors the family's own `raw` module
//! uses), so the module carries a local `allow` under the crate-wide `deny`.
#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::{load_file, BsGetArgs, Export, Function, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{signed_value, Atom, Eterm};

use super::raw;
use crate::context::{ExecutionContext, MinimalContext};
use crate::machine::Machine;
use crate::terms::{binary_bytes, make_heap_binary};

// ---------------------------------------------------------------------------
// Synthetic-module fixture (the `funs/tests.rs` pattern: build the IR by hand)
// ---------------------------------------------------------------------------

/// A one-module fixture with an owned atom table, built without the loader.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("bitsrun_synth");
        Fixture { module }
    }

    /// Intern `name` and return its atom.
    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Export `name/arity` at `label` (what `run_export` resolves).
    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    /// Append a function; returns its entry label (the first label in `code`).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let mut labels = BTreeMap::new();
        for (index, instr) in code.iter().enumerate() {
            if let Instr::Label(label) = instr {
                labels.insert(*label, index);
            }
        }
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn finish(self) -> Module {
        self.module
    }
}

// ---------------------------------------------------------------------------
// Result readers
// ---------------------------------------------------------------------------

/// The exact bytes and **bit count** of a result bitstring, byte-aligned or not.
///
/// [`binary_bytes`] cannot read a non-byte-aligned bitstring by design, so the
/// exact-size assertions go through the frozen reader instead.
fn bitstring(term: Eterm) -> (Vec<u8>, usize) {
    // SAFETY: `term` is a live bitstring the engine just produced; the arena the
    // context owns outlives this call (the context is borrowed for the whole test).
    let (base, offset, size) = unsafe { beam_term::bitstring_parts(term) };
    let mut out = vec![0u8; size.div_ceil(8)];
    for i in 0..size {
        let bit_index = offset + i;
        // SAFETY: the base pointer covers `offset + size` bits by construction, so
        // the byte holding bit `bit_index` is inside the bitstring's storage.
        let bit = (unsafe { *base.add(bit_index / 8) } >> (7 - (bit_index % 8))) & 1;
        out[i / 8] |= bit << (7 - (i % 8));
    }
    (out, size)
}

/// Build a refc-binary term over `payload_bytes` whose window is
/// `[offset_bits, offset_bits + size_bits)`.
///
/// The term is the frozen 8-word `[BinRef][ErlSubBits]` pair of
/// [`beam_term::refc::write_refc_binary`] allocated from the context's arena. The
/// returned `Vec<Eterm>` is the **word-aligned backing store** the pair's
/// `BinRef.val` points into: the caller must keep it alive (and unmoved) for as long
/// as the term is read — the frozen `write_refc_binary` ownership contract — hence
/// it is returned rather than dropped.
fn refc_window(
    ctx: &mut MinimalContext,
    payload_bytes: &[u8],
    offset_bits: usize,
    size_bits: usize,
) -> (Eterm, Vec<Eterm>) {
    let words = payload_bytes
        .len()
        .div_ceil(core::mem::size_of::<Eterm>())
        .max(1);
    let mut payload: Vec<Eterm> = vec![Eterm::default(); words];
    // SAFETY: `payload` owns `words * sizeof(Eterm) >= payload_bytes.len()` bytes, so
    // the byte copy stays inside its own allocation.
    unsafe {
        core::ptr::copy_nonoverlapping(
            payload_bytes.as_ptr(),
            payload.as_mut_ptr().cast::<u8>(),
            payload_bytes.len(),
        );
    }
    let base = payload.as_ptr().cast::<u8>();
    let val = payload.as_mut_ptr();
    let hp = ctx
        .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
        .expect("the arena hands out ERL_REFC_BITS_SIZE words");
    // SAFETY: `hp` points at `ERL_REFC_BITS_SIZE` (8) exclusively-owned words in the
    // context's pointer-stable arena, `val`/`base` point at the live, word-aligned
    // `payload` (`write_refc_binary`'s alignment contract), and `next` is null (the
    // chain terminator). The term is read only while `payload`/`ctx` live.
    let term = unsafe {
        beam_term::refc::write_refc_binary(
            hp,
            val,
            core::ptr::null_mut(),
            base,
            offset_bits,
            size_bits,
        )
    };
    (term, payload)
}

// ---------------------------------------------------------------------------
// (1) a refc subject whose window does not start on a byte boundary
// ---------------------------------------------------------------------------

/// The `RawPayload`/`RawSubBits` shape every case below is built over: the bytes
/// `[0b1010_1100, 0b0011_0101, 0b1000_0000]` read through a window of bits `1..17`
/// are exactly `0b0101_1000_0110_1011` = `0x58 0x6B`.
const WINDOW_PAYLOAD: [u8; 3] = [0b1010_1100, 0b0011_0101, 0b1000_0000];
/// The 16 bits the `1..17` window of [`WINDOW_PAYLOAD`] holds.
const WINDOW_BITS: [u8; 2] = [0x58, 0x6B];

/// `f/1` = `[bs_start_match3 X0 -> X1, bs_get_tail X1 -> X0, return]`, driven through
/// [`Machine::run_label`] with `x(0)` = a refc binary whose window starts at bit 1.
///
/// This is the exact code path the fix opened: before it, `raw::heap_bits` refused a
/// non-byte-aligned refc window, so `bs_start_match3` would have taken the `no_fail`
/// raise and the run would have been a typed error instead of the window's bits.
#[test]
fn non_byte_aligned_refc_window_reads_end_to_end_via_bs_get_tail() {
    let mut fixture = Fixture::new();
    let code = vec![
        Instr::Label(Label(1)),
        Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        Instr::BsGetTail {
            ctx: Operand::X(1),
            dst: Operand::X(0),
            live: 2,
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let (subject, _payload) = refc_window(&mut ctx, &WINDOW_PAYLOAD, 1, 16);
    // The subject really is the off-heap form this case is about.
    // SAFETY: `subject` is a live bitstring built on the line above.
    assert!(unsafe { beam_term::refc::is_refc_binary(subject) });

    let result = machine
        .run_label(&module, entry, 1, &[subject], &mut ctx)
        .expect("bs_get_tail over a non-byte-aligned refc window");
    assert_eq!(
        bitstring(result),
        (WINDOW_BITS.to_vec(), 16),
        "the returned bitstring is exactly the payload's bits 1..17"
    );
}

/// `f/1` = `[bs_start_match3 X0 -> X1, bs_get_integer2 … size 8 … X0, return]`: the
/// same non-byte-aligned refc subject read through the fixed-integer opcode instead.
///
/// The first 8 bits of the `1..17` window are `0b0101_1000` = `88`.
#[test]
fn non_byte_aligned_refc_window_reads_end_to_end_via_bs_get_integer2() {
    let mut fixture = Fixture::new();
    let code = vec![
        Instr::Label(Label(1)),
        Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        Instr::BsGetInteger2(Box::new(BsGetArgs {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            live: 2,
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
            dst: Operand::X(0),
        })),
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let (subject, _payload) = refc_window(&mut ctx, &WINDOW_PAYLOAD, 1, 16);

    let result = machine
        .run_label(&module, entry, 1, &[subject], &mut ctx)
        .expect("bs_get_integer2 over a non-byte-aligned refc window");
    assert_eq!(signed_value(result), 0b0101_1000, "the window's first 8 bits");
}

// ---------------------------------------------------------------------------
// (2) an `append`/`private_append` segment that is not the first
// ---------------------------------------------------------------------------

/// The `[Index, Unit, Flags, Value, Size]` operand group of an `integer` segment
/// writing `value : size` (the leading `Type` operand is the caller's `integer`
/// atom).
fn integer_segment(value: i64, size: i64) -> Vec<Operand> {
    vec![
        Operand::Integer(1), // Index (a compiler-side identifier)
        Operand::Integer(1), // Unit
        Operand::Nil,        // Flags (no little/signed/native)
        Operand::Integer(value),
        Operand::Integer(size),
    ]
}

/// `f/1` = `[bs_create_bin X0 -> X0]` whose segment list is
/// `[integer,1,1,nil,7,8] [<ty>,2,1,nil,X0,all]`, driven through
/// [`Machine::run_export`].
///
/// The integer segment writes `7:8`, then the appending segment concatenates the
/// whole of `x(0)` onto the accumulated bits, so the result is `<<7:8, Src/bits>>`.
/// Before the fix, the engine rejected an appending segment in a non-first position
/// outright (`bs_create_bin: append/private_append is not the first segment`).
fn run_non_first_append(
    ty: &str,
    src: Eterm,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
) -> Eterm {
    let mut fixture = Fixture::new();
    // Intern every atom the segment walk resolves before any operand is built.
    let integer_atom = fixture.atom("integer");
    let ty_atom = fixture.atom(ty);
    let all_atom = fixture.atom("all");

    let mut segments = Vec::new();
    // segment 0 — `integer,1,1,nil,7,8`.
    segments.push(Operand::Atom(integer_atom));
    segments.extend(integer_segment(7, 8));
    // segment 1 — `<ty>,2,1,nil,{x,0},all`.
    segments.push(Operand::Atom(ty_atom));
    segments.push(Operand::Integer(2)); // Index
    segments.push(Operand::Integer(1)); // Unit
    segments.push(Operand::Nil); // Flags
    segments.push(Operand::X(0)); // Value — the source bitstring
    segments.push(Operand::Atom(all_atom)); // Size

    let code = vec![
        Instr::Label(Label(1)),
        Instr::BsCreateBin {
            fail: NO_FAIL,
            alloc: 0,
            live: 1,
            unit: 1,
            dst: Operand::X(0),
            segments,
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 1, code);
    fixture.add_export("f", 1, entry);
    let module = fixture.finish();
    let function = module
        .atom_table
        .lookup("f")
        .expect("the fixture interned `f`");

    machine
        .run_export(&module, function, 1, &[src], ctx)
        .unwrap_or_else(|e| panic!("{ty} as a non-first segment: {e:?}"))
}

/// A non-first `append` of a heap-binary source through
/// [`Machine::run_export`]: `<<7:8, 16#AB, 16#CD>>`.
#[test]
fn a_non_first_append_segment_concatenates_via_run_export() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let src = make_heap_binary(&mut ctx, &[0xAB, 0xCD]).expect("a heap binary source");
    let result = run_non_first_append("append", src, &mut machine, &mut ctx);
    assert_eq!(bitstring(result), (vec![0x07, 0xAB, 0xCD], 24));
    assert_eq!(
        binary_bytes(result).expect("a byte-aligned result"),
        &[0x07, 0xAB, 0xCD],
        "the byte-aligned view agrees with the exact-bit reader"
    );
}

/// A non-first `private_append` whose source is itself a **65-byte refc binary**:
/// the accumulated `7:8` plus the whole 520-bit payload, i.e. a 528-bit (66-byte)
/// result that `raw::make_binary` stores off-heap again.
#[test]
fn a_non_first_private_append_segment_seeds_from_a_refc_binary() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let payload: Vec<u8> = (1..=65u8).collect();
    let src = raw::make_binary(&mut machine, &mut ctx, &payload, payload.len() * 8)
        .expect("a 65-byte refc source");
    // SAFETY: `src` is a live bitstring built on the line above.
    assert!(unsafe { beam_term::refc::is_refc_binary(src) });

    let result = run_non_first_append("private_append", src, &mut machine, &mut ctx);
    let mut want = vec![0x07u8];
    want.extend_from_slice(&payload);
    assert_eq!(bitstring(result), (want, 8 + payload.len() * 8));
}

// ---------------------------------------------------------------------------
// (3) the live `erlc`/`erl` oracle cross-check (skips loudly when absent)
// ---------------------------------------------------------------------------

/// The inline fixture compiled at test time. Every function is **BIF-free** (the
/// [`MinimalContext`] installs no BIF hook), so they exercise the bitstring family
/// and nothing else:
///
/// * `first3/1` reads a 3-bit field off a byte-aligned subject;
/// * `tail16/1` skips one bit and reads the next 16, so a `>64`-byte subject (which
///   the harness builds as an off-heap refc binary) is read at a non-byte-aligned
///   window;
/// * `rest16/1` skips one bit and 16 more, then takes the whole remaining tail — over
///   a 70-byte subject the 543-bit result is above the 64-byte on-heap limit, so it
///   must come back as an off-heap refc binary;
/// * `rest16bin/1` skips 16 byte-aligned bits and takes the rest as a `binary` — over
///   a 70-byte subject the 544-bit (68-byte) result is likewise above the limit;
/// * `built_tail/1` builds the 70-byte subject **itself** at runtime (`<<Seed:560>>`,
///   an `integer` `bs_create_bin` segment wider than 64 bits) and reads a tail at a
///   non-byte-aligned window out of it.
const ORACLE_SRC: &str = "\
-module(bitsrun).
-export([first3/1, tail16/1, rest16/1, rest16bin/1, built_tail/1]).

first3(Bin) ->
    <<A:3, _/bits>> = Bin,
    A.

tail16(Bin) ->
    <<_:1, B:16, _/bits>> = Bin,
    B.

rest16(Bin) ->
    <<_:1, _:16, Rest/bits>> = Bin,
    Rest.

rest16bin(Bin) ->
    <<_:16, Rest/binary>> = Bin,
    Rest.

built_tail(Seed) ->
    Bin = <<Seed:560>>,
    <<_:1, _:16, Rest/bits>> = Bin,
    Rest.
";

/// Compile [`ORACLE_SRC`] into a per-`tag` fresh temp dir and return that dir.
///
/// `None` means `erlc` could not be **spawned** at all (the caller skips loudly). An
/// `erlc` that *ran* and exited non-zero is a **harness** failure (the source is
/// valid Erlang), so it panics with the captured stderr rather than masquerading as
/// an absent toolchain.
fn compile_oracle(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_bitstring_run_{tag}"));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("bitsrun.erl"), ORACLE_SRC)
        .unwrap_or_else(|error| panic!("write bitsrun.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("bitsrun.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling bitsrun.erl\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir)
}

/// Run `eval` under the system `erl` with `dir` **appended** to the code path and
/// return its stdout.
///
/// `-pz`, never `-pa`: appending keeps the real stdlib ahead of any fixture module in
/// `dir`. `None` means no runnable `erl` at all (the caller skips loudly); a spawned
/// `erl` that exits non-zero is a harness failure and panics with the stderr.
fn erl_eval(dir: &Path, eval: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pz")
        .arg(dir)
        .arg("-eval")
        .arg(eval)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `{eval}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The bytes of a binary literal, for the inline `erl -eval` source.
fn byte_literal(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| byte.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Load the compiled fixture's export atom.
fn oracle_export(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("the fixture exports {name:?}"))
}

/// `bitsrun:first3/1` over `<<16#E5>>` (bits `1110_0101`): the first three bits are
/// `111` = `7`. The pinned value is cross-checked against the live `erl` on the same
/// byte.
#[test]
fn first3_over_a_byte_aligned_subject_matches_the_oracle() {
    let Some(dir) = compile_oracle("first3") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bitsrun:first3/1 cross-check skipped");
        return;
    };
    let module = load_file(dir.join("bitsrun.beam"))
        .unwrap_or_else(|e| panic!("load bitsrun.beam: {e:?}"));

    let bytes = [0xE5u8];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = make_heap_binary(&mut ctx, &bytes).expect("a heap binary");
    let result = machine
        .run_export(&module, oracle_export(&module, "first3"), 1, &[subject], &mut ctx)
        .expect("bitsrun:first3/1");
    assert_eq!(signed_value(result), 7, "bitsrun:first3(<<16#E5>>)");

    let eval = format!(
        "io:format(\"~p\", [bitsrun:first3(<<{}>>)]), halt(0).",
        byte_literal(&bytes)
    );
    match erl_eval(&dir, &eval) {
        Some(out) => assert_eq!(out.trim(), "7", "the oracle disagrees for `{eval}`"),
        None => eprintln!("SKIP: no system `erl` on PATH -- first3/1 oracle value skipped"),
    }
}

/// `bitsrun:tail16/1` over a **70-byte** subject (bits `1..17`): the payload is
/// `1..=70`, so the 16-bit field `B` is `0x0204` = `516`. The subject is built as an
/// off-heap refc binary (above the 64-byte heap limit), which is the shape the
/// non-aligned refc read path is about, and the pinned value is cross-checked
/// against the live `erl` on the same bytes.
#[test]
fn tail16_over_a_refc_subject_matches_the_oracle() {
    let Some(dir) = compile_oracle("tail16") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bitsrun:tail16/1 cross-check skipped");
        return;
    };
    let module = load_file(dir.join("bitsrun.beam"))
        .unwrap_or_else(|e| panic!("load bitsrun.beam: {e:?}"));

    let payload: Vec<u8> = (1..=70u8).collect();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let subject = raw::make_binary(&mut machine, &mut ctx, &payload, payload.len() * 8)
        .expect("a 70-byte refc subject");
    // SAFETY: `subject` is a live bitstring built on the line above.
    assert!(
        unsafe { beam_term::refc::is_refc_binary(subject) },
        "70 bytes is above the 64-byte on-heap limit"
    );

    let result = machine
        .run_export(&module, oracle_export(&module, "tail16"), 1, &[subject], &mut ctx)
        .expect("bitsrun:tail16/1");
    assert_eq!(
        signed_value(result),
        0x0204,
        "bitsrun:tail16 over the 1..=70 payload"
    );

    let eval = format!(
        "io:format(\"~p\", [bitsrun:tail16(<<{}>>)]), halt(0).",
        byte_literal(&payload)
    );
    match erl_eval(&dir, &eval) {
        Some(out) => assert_eq!(out.trim(), "516", "the oracle disagrees for `{eval}`"),
        None => eprintln!("SKIP: no system `erl` on PATH -- tail16/1 oracle value skipped"),
    }
}

/// The bits `[skip, 8 * payload.len())` of `payload`, as `(bytes, bit_count)` — the
/// value `<<_:skip, Rest/bits>>` must return.
///
/// Pure Rust over the raw subject bytes, so the expectation is independent of the
/// engine's own [`bitstring`] reader (the payload is `1..=70`, whose shifted window is
/// not a plain byte slice).
fn tail_of(payload: &[u8], skip: usize) -> (Vec<u8>, usize) {
    let nbits = payload.len() * 8 - skip;
    let mut out = vec![0u8; nbits.div_ceil(8)];
    for i in 0..nbits {
        let bit = (payload[(skip + i) / 8] >> (7 - ((skip + i) % 8))) & 1;
        out[i / 8] |= bit << (7 - (i % 8));
    }
    (out, nbits)
}

/// Build the 70-byte (`1..=70`) off-heap refc subject `rest16/1` and `rest16bin/1`
/// read, in the machine that owns its payload: returns the subject term and the
/// payload bytes it was built from.
fn refc_subject_70(machine: &mut Machine, ctx: &mut MinimalContext) -> (Eterm, Vec<u8>) {
    let payload: Vec<u8> = (1..=70u8).collect();
    let subject = raw::make_binary(machine, ctx, &payload, payload.len() * 8)
        .expect("a 70-byte refc subject");
    // SAFETY: `subject` is a live bitstring built on the line above.
    assert!(
        unsafe { beam_term::refc::is_refc_binary(subject) },
        "70 bytes is above the 64-byte on-heap limit"
    );
    (subject, payload)
}

/// `bitsrun:rest16/1` = `<<_:1, _:16, Rest/bits>>` over a **70-byte** subject: the
/// 543-bit result is above the 64-byte on-heap limit, so it must come back as an
/// off-heap refc binary.
///
/// The C `get_tail` match command builds through `erts_build_sub_bitstring`, which has
/// no size limit; the expected value is the subject's bits `17..560` (derived in Rust
/// by [`tail_of`]) and the bit count is cross-checked against the live `erl` on the same
/// bytes.
///
/// A literal `<<_:1, _:16, Rest/binary>>` cannot serve here: the 17-bit prefix leaves
/// 543 bits, which is not a multiple of the unit 8 a `binary` tail requires, so the
/// oracle `badmatch`es it (`ensure_at_least 17 8`). `rest16bin/1` is the byte-aligned
/// `Rest/binary` counterpart below.
#[test]
fn rest16_over_a_refc_subject_returns_an_off_heap_result() {
    let Some(dir) = compile_oracle("rest16") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bitsrun:rest16/1 cross-check skipped");
        return;
    };
    let module = load_file(dir.join("bitsrun.beam"))
        .unwrap_or_else(|e| panic!("load bitsrun.beam: {e:?}"));

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let (subject, payload) = refc_subject_70(&mut machine, &mut ctx);

    let result = machine
        .run_export(&module, oracle_export(&module, "rest16"), 1, &[subject], &mut ctx)
        .expect("bitsrun:rest16/1");
    // SAFETY: `result` is a live bitstring the run just produced.
    assert!(
        unsafe { beam_term::refc::is_refc_binary(result) },
        "a 543-bit result is above the 64-byte on-heap limit"
    );
    let (want, want_bits) = tail_of(&payload, 17);
    assert_eq!(want_bits, 543, "70 bytes minus a 17-bit prefix");
    assert_eq!(
        bitstring(result),
        (want, want_bits),
        "bitsrun:rest16 is the subject's bits 17..560, bit-exactly"
    );

    let eval = format!(
        "io:format(\"~p\", [bit_size(bitsrun:rest16(<<{}>>))]), halt(0).",
        byte_literal(&payload)
    );
    match erl_eval(&dir, &eval) {
        Some(out) => assert_eq!(out.trim(), "543", "the oracle disagrees for `{eval}`"),
        None => eprintln!("SKIP: no system `erl` on PATH -- rest16/1 oracle value skipped"),
    }
}

/// `bitsrun:rest16bin/1` = `<<_:16, Rest/binary>>` over the same 70-byte subject: the
/// 544-bit (68-byte) `Rest/binary` result is above the 64-byte on-heap limit, so it too
/// must be an off-heap refc binary, and its bytes are exactly the subject's bytes
/// `2..70`.
#[test]
fn rest16bin_over_a_refc_subject_returns_an_off_heap_binary_result() {
    let Some(dir) = compile_oracle("rest16bin") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bitsrun:rest16bin/1 cross-check skipped");
        return;
    };
    let module = load_file(dir.join("bitsrun.beam"))
        .unwrap_or_else(|e| panic!("load bitsrun.beam: {e:?}"));

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let (subject, payload) = refc_subject_70(&mut machine, &mut ctx);

    let result = machine
        .run_export(&module, oracle_export(&module, "rest16bin"), 1, &[subject], &mut ctx)
        .expect("bitsrun:rest16bin/1");
    // SAFETY: `result` is a live binary the run just produced.
    assert!(
        unsafe { beam_term::refc::is_refc_binary(result) },
        "a 544-bit (68-byte) result is above the 64-byte on-heap limit"
    );
    let (want, want_bits) = tail_of(&payload, 16);
    assert_eq!(want_bits, 544, "70 bytes minus a byte-aligned 16-bit prefix");
    assert_eq!(
        bitstring(result),
        (want, want_bits),
        "bitsrun:rest16bin is the subject's bits 16..560, bit-exactly"
    );
    assert_eq!(
        binary_bytes(result).expect("a byte-aligned binary"),
        &payload[2..],
        "the byte-aligned view is the subject's bytes 2..70"
    );

    let eval = format!(
        "io:format(\"~p\", [bit_size(bitsrun:rest16bin(<<{}>>))]), halt(0).",
        byte_literal(&payload)
    );
    match erl_eval(&dir, &eval) {
        Some(out) => assert_eq!(out.trim(), "544", "the oracle disagrees for `{eval}`"),
        None => eprintln!("SKIP: no system `erl` on PATH -- rest16bin/1 oracle value skipped"),
    }
}

/// The 560-bit big-endian representation of `seed` — the 70-byte field the module's own
/// `<<Seed:560>>` must build.
fn seed_560(seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; 70];
    out[62..].copy_from_slice(&seed.to_be_bytes());
    out
}

/// `bitsrun:built_tail/1` builds the 70-byte subject **itself** at runtime with a
/// `bs_create_bin` `integer` segment **wider than 64 bits** (`<<Seed:560>>`), then reads
/// a non-byte-aligned window out of it: the strictly stronger end-to-end shape (no
/// harness-built subject, and the wide-segment construction is exercised too).
///
/// The expected value is the subject's bits `17..560`, where the subject is the 560-bit
/// big-endian image of `Seed` ([`seed_560`]); the bit count is cross-checked against the
/// live `erl`, which also builds the subject the same way.
#[test]
fn built_tail_builds_the_wide_subject_itself_and_returns_an_off_heap_result() {
    let Some(dir) = compile_oracle("built_tail") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bitsrun:built_tail/1 cross-check skipped");
        return;
    };
    let module = load_file(dir.join("bitsrun.beam"))
        .unwrap_or_else(|e| panic!("load bitsrun.beam: {e:?}"));

    /// 57 bits, so it fits in a 60-bit immediate — how the oracle passes it too.
    const SEED: u64 = 0x0102_0304_0506_0708;

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let seed = beam_term::make_small(SEED as i64);

    let result = machine
        .run_export(
            &module,
            oracle_export(&module, "built_tail"),
            1,
            &[seed],
            &mut ctx,
        )
        .expect("bitsrun:built_tail/1");
    // SAFETY: `result` is a live bitstring the run just produced.
    assert!(
        unsafe { beam_term::refc::is_refc_binary(result) },
        "a 543-bit result is above the 64-byte on-heap limit"
    );
    let (want, want_bits) = tail_of(&seed_560(SEED), 17);
    assert_eq!(want_bits, 543, "560 bits minus a 17-bit prefix");
    assert_eq!(
        bitstring(result),
        (want, want_bits),
        "bitsrun:built_tail is the module-built subject's bits 17..560, bit-exactly"
    );

    let eval = format!(
        "Bin = <<{SEED}:560>>, <<_:1, _:16, Rest/bits>> = Bin, \
         io:format(\"~p\", [bit_size(Rest)]), halt(0)."
    );
    match erl_eval(&dir, &eval) {
        Some(out) => assert_eq!(out.trim(), "543", "the oracle disagrees for `{eval}`"),
        None => eprintln!("SKIP: no system `erl` on PATH -- built_tail/1 oracle value skipped"),
    }
}
