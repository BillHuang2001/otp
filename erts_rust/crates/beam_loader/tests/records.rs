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

//! Integration tests for the `Recs` chunk — a module's compile-time **native-record
//! definitions** — over the committed oracle fixture `tests/fixtures/records_fixture.beam`.
//!
//! `Recs` is what the `records:get_definition/2` BIF serves; `beam_loader` parses it
//! into [`beam_loader::RecordDefinition`]s (see `src/chunk/record.rs` for the wire
//! layout and the `u,0`/`i,0` default-tag distinction, and `tests/fixtures/README.md`
//! for the verified fixture summary).
//!
//! # What this proves
//!
//! * **The fixture's four records are parsed in declaration order** with the right
//!   `is_exported` flag and the right field lists.
//! * **Every default encoding is materialized as the right term**: the small-integer
//!   `0` default of `#mixed.a` is distinct from an *absent* field (`None`), an atom
//!   default, a tuple literal, a list literal, and a bignum (the `LitT`-backed and
//!   *synthesized*-literal paths).
//! * **The parsed definitions reproduce the oracle's `records:get_definition/2`
//!   output.** The expected values are the pinned strings the system `erl`
//!   (OTP 29 / erts-17.1) returns for this fixture — see [`ORACLE`]. The
//!   [`the_live_oracle_agrees_with_the_pinned_definitions`] test re-derives them from
//!   a live `erl` (skipping **loudly** when the toolchain is absent), so the pinned
//!   facts can never silently drift from the oracle; the per-record tests below
//!   assert the parse against those same facts. Chain: `parse ≡ pinned ≡ live oracle`.
//! * **A malformed `Recs` chunk is a typed `BeamError`, never a panic** — the
//!   negative cases go through the whole [`beam_loader::load`] pipeline on a
//!   hand-built image.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use beam_codec::term::{big_parts, cons_head, cons_tail, tuple_arity, tuple_element};
use beam_common::BeamError;
use beam_loader::{load, load_file, Atom, Module, RecordDefinition, RecordField};
use beam_term::prelude::{atom_val, is_atom, is_nil, is_small, signed_value, Eterm};

/// Loads the committed `records_fixture.beam` oracle fixture.
fn load_records() -> Module {
    load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/records_fixture.beam"
    ))
    .expect("the records oracle fixture must load")
}

/// The module-local atom for `name`.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("the fixture must intern the atom `{name}`"))
}

/// The name of a field's atom (panics when the atom is not in the module's table).
fn field_name(module: &Module, field: &RecordField) -> String {
    module
        .atom_name(field.name)
        .unwrap_or_else(|| panic!("field atom {:?} is not in the module's table", field.name))
        .to_string()
}

/// The `(field name, default)` view of a definition, in declaration order.
fn field_view(module: &Module, definition: &RecordDefinition) -> Vec<(String, Option<Eterm>)> {
    definition
        .fields
        .iter()
        .map(|field| (field_name(module, field), field.default))
        .collect()
}

/// The declared field names of a definition, in declaration order.
fn declared_names(module: &Module, definition: &RecordDefinition) -> Vec<String> {
    definition
        .fields
        .iter()
        .map(|field| field_name(module, field))
        .collect()
}

/// The name of a term that must be an atom.
fn atom_name(module: &Module, term: Eterm) -> String {
    assert!(is_atom(term), "expected an atom term, got {term:?}");
    module
        .atom_name(atom_val(term))
        .unwrap_or_else(|| panic!("atom {:?} is not in the module's table", atom_val(term)))
        .to_string()
}

/// The value of a term that must be a small integer.
fn small_value(term: Eterm) -> i64 {
    assert!(is_small(term), "expected a small integer, got {term:?}");
    signed_value(term)
}

/// The decimal value of a term that must be a bignum, as a `u128` (the fixture's only
/// bignum, `10^25`, fits).
fn bignum_u128(term: Eterm) -> (bool, u128) {
    let (negative, limbs) = big_parts(term).expect("expected a bignum term");
    let mut value: u128 = 0;
    for limb in limbs.iter().rev() {
        value = (value << 64) | u128::from(*limb);
    }
    (negative, value)
}

// ---------------------------------------------------------------------------
// Parsed definitions
// ---------------------------------------------------------------------------

#[test]
fn the_four_records_are_parsed_in_declaration_order() {
    let module = load_records();

    let names: Vec<String> = module
        .records()
        .iter()
        .map(|record| {
            module
                .atom_name(record.name)
                .unwrap_or_else(|| panic!("record name {:?} is not in the table", record.name))
                .to_string()
        })
        .collect();
    assert_eq!(names, ["user", "empty", "mixed", "shared"]);

    // `#shared` is the only exported record (`-export_record([shared])`).
    let exported: Vec<bool> = module.records().iter().map(|r| r.is_exported).collect();
    assert_eq!(exported, [false, false, false, true]);
}

/// `Module::record_by_name` resolves a record through the module's atom table — the
/// lookup the `records:get_definition/2` BIF needs.
#[test]
fn records_resolve_by_name_and_by_atom() {
    let module = load_records();

    let user = module.record_by_name("user").expect("`user` is defined");
    assert_eq!(user.name, atom(&module, "user"));
    // The atom-based lookup returns the same definition.
    assert_eq!(module.record(atom(&module, "user")), Some(user));

    // A name that is not a record resolves to `None` (by name) …
    assert!(module.record_by_name("nope").is_none());
    // … and so does a *known* atom that names no record: `noop/0` is exported by the
    // fixture, so its atom is in the module's table but is not a record name.
    let noop = atom(&module, "noop");
    assert!(
        !module.records().iter().any(|record| record.name == noop),
        "`noop` must not be a record name"
    );
    assert_eq!(module.record(noop), None);
}

#[test]
fn the_user_record_has_the_oracle_field_list() {
    let module = load_records();
    let user = module.record_by_name("user").expect("`user` is defined");

    assert!(!user.is_exported);
    assert_eq!(declared_names(&module, user), ["id", "name", "city"]);

    // `[{id, -1}, name, city]` — `id` has the default `-1`; `name`/`city` are absent.
    let fields = field_view(&module, user);
    assert_eq!(
        fields[0].0, "id",
    );
    assert_eq!(small_value(fields[0].1.expect("`id` has a default")), -1);
    assert_eq!(fields[1], ("name".to_string(), None));
    assert_eq!(fields[2], ("city".to_string(), None));
}

#[test]
fn the_empty_record_has_no_fields() {
    let module = load_records();
    let empty = module.record_by_name("empty").expect("`empty` is defined");

    assert!(!empty.is_exported);
    assert!(empty.fields.is_empty());
    assert_eq!(field_view(&module, empty), Vec::<(String, Option<Eterm>)>::new());
}

#[test]
fn the_mixed_record_reproduces_every_default_encoding() {
    let module = load_records();
    let mixed = module.record_by_name("mixed").expect("`mixed` is defined");

    assert!(!mixed.is_exported);
    assert_eq!(
        declared_names(&module, mixed),
        ["a", "b", "c", "d", "e", "f"]
    );

    let fields = field_view(&module, mixed);

    // `a = 0` — an **`i`-tagged** default, i.e. `Some(0)`, *not* `None`. This is the
    // whole reason defaults are decoded from the raw tag: `u,0` (absent) and `i,0`
    // (default `0`) share a magnitude and differ only by tag.
    assert_eq!(small_value(fields[0].1.expect("`a` has a default of 0")), 0);

    // `b` — absent (the `u,0` sentinel).
    assert_eq!(fields[1], ("b".to_string(), None));

    // `c = undefined` — an atom default.
    assert_eq!(atom_name(&module, fields[2].1.expect("`c` has a default")), "undefined");

    // `d = {1, 2}` — a tuple literal (`LitT` literal 0).
    let tuple = fields[3].1.expect("`d` has a default");
    assert_eq!(tuple_arity(tuple).expect("`d` is a tuple"), 2);
    assert_eq!(small_value(tuple_element(tuple, 0).expect("element 0")), 1);
    assert_eq!(small_value(tuple_element(tuple, 1).expect("element 1")), 2);

    // `e = [a, b]` — a list literal (`LitT` literal 1).
    let list = fields[4].1.expect("`e` has a default");
    assert_eq!(atom_name(&module, cons_head(list).expect("`e` is a cons cell")), "a");
    let tail = cons_tail(list).expect("`e` is a cons cell");
    assert_eq!(atom_name(&module, cons_head(tail).expect("`e` is a 2-list")), "b");
    assert!(is_nil(cons_tail(tail).expect("`e` ends in nil")));

    // `f = 10000000000000000000000000` — a **wide `i` field**, i.e. a bignum default.
    // It is not in the file's `LitT`; the loader appends it to the synthesized-literal
    // set, so the field default is exactly the module's last literal.
    let big_default = fields[5].1.expect("`f` has a default");
    let (negative, value) = bignum_u128(big_default);
    assert!(!negative);
    assert_eq!(value, 10u128.pow(25));
    assert_eq!(
        module.literals().len(),
        3,
        "2 file `LitT` literals plus the synthesized `f` bignum"
    );
    assert_eq!(module.literal(2), Some(big_default));
}

#[test]
fn the_shared_record_is_exported() {
    let module = load_records();
    let shared = module.record_by_name("shared").expect("`shared` is defined");

    assert!(shared.is_exported);
    assert_eq!(declared_names(&module, shared), ["key", "value"]);

    let fields = field_view(&module, shared);
    // `[key, {value, none}]`.
    assert_eq!(fields[0], ("key".to_string(), None));
    assert_eq!(fields[1].0, "value");
    assert_eq!(
        atom_name(&module, fields[1].1.expect("`value` has the default `none`")),
        "none"
    );
}

// ---------------------------------------------------------------------------
// Oracle cross-check
// ---------------------------------------------------------------------------

/// The pinned `records:get_definition(records_fixture, Name)` results, captured from
/// the system `erl` (OTP 29 / erts-17.1), with all whitespace removed.
///
/// `records:get_definition/2` returns `{#{is_exported => Bool}, Fields}`, `Fields` in
/// **declaration order**, each field `{Name, Default}` when it has a default and the
/// bare atom `Name` otherwise. The anonymous record field of `#mixed` and the
/// one-key map order are the oracle's own output, verbatim.
const ORACLE: &[(&str, &str)] = &[
    ("user", "{#{is_exported=>false},[{id,-1},name,city]}"),
    ("empty", "{#{is_exported=>false},[]}"),
    (
        "mixed",
        "{#{is_exported=>false},[{a,0},b,{c,undefined},{d,{1,2}},{e,[a,b]},\
         {f,10000000000000000000000000}]}",
    ),
    ("shared", "{#{is_exported=>true},[key,{value,none}]}"),
];

/// Strips every whitespace character (the oracle's `~p` wraps long terms at 80
/// columns; none of this fixture's terms needs significant whitespace).
fn normalize(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The wall-clock bound for one oracle `erl`/`erlc` invocation.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs `command`, waiting at most `limit` for it to exit.
///
/// A process that cannot be spawned yields its `io::Error`, so a caller skips
/// **loudly** when the toolchain is absent (matching `tests/real_modules.rs`). A
/// process that does not exit within `limit` is killed and this **panics**: the
/// oracle is a real `erl`/`erlc`, and an invocation that never terminates must fail
/// CI rather than hang it forever. Outputs here are tiny (well under a pipe buffer),
/// so reaping the child before draining its pipes cannot deadlock.
fn run_bounded(command: &mut Command, limit: Duration) -> std::io::Result<std::process::Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the bounded oracle command did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Compiles the committed fixture with the **system** `erlc` and asks the **system**
/// `erl` for `records:get_definition/2`, asserting the live output matches the pinned
/// [`ORACLE`] constants.
///
/// The fixture module is named `records_fixture`, deliberately **not** `records`: an
/// earlier `-module(records)` collided with the stdlib `records` module, so loading it
/// replaced the stdlib one and the oracle's `records:get_definition/2` hung forever.
///
/// A missing `erl`/`erlc` (or a toolchain that rejects the fixture) skips **loudly** —
/// it never fails in that case, matching `tests/real_modules.rs`.
#[test]
fn the_live_oracle_agrees_with_the_pinned_definitions() {
    let dir = unique_temp_dir("records_fixture");

    // Compile the committed fixture source with the system compiler (the same command
    // `tests/fixtures/README.md` documents).
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/records_fixture.erl");
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("SKIP: cannot create {} for the oracle cross-check", dir.display());
        return;
    }
    if let Err(error) = std::fs::copy(&source, dir.join("records_fixture.erl")) {
        eprintln!("SKIP: cannot stage {} ({error})", source.display());
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let mut erlc = Command::new("erlc");
    erlc.arg("+deterministic")
        .arg("records_fixture.erl")
        .current_dir(&dir);
    match run_bounded(&mut erlc, ORACLE_TIMEOUT) {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "SKIP: `erlc` rejected the fixture source ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        Err(error) => {
            eprintln!("SKIP: cannot run `erlc` ({error}) — need the system OTP compiler");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }

    // `records:get_definition/2` is a BIF: it reads the *loaded* module's `Recs`
    // chunk, so the fixture must be loaded under its own (uniquely named) module.
    let eval = "{module,records_fixture}=code:load_file(records_fixture),\
                lists:foreach(fun(R) -> \
                    io:format(\"~s@~p@;\", [R, records:get_definition(records_fixture, R)]) \
                end, [user, empty, mixed, shared]),\
                halt().";
    let mut erl = Command::new("erl");
    erl.args(["-noshell", "-pa"])
        .arg(&dir)
        .args(["-eval", eval]);
    let output = match run_bounded(&mut erl, ORACLE_TIMEOUT) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("SKIP: cannot run `erl` ({error}) — need the system OTP runtime");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "the oracle `erl` failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );

    // The eval emits `name@<~p output>@;` per record — a trailing `@` before each
    // `;`. Whitespace-normalize the whole output first (the `~p` line-wrapping is
    // insignificant for these terms), drop the trailing `@`, then split on the marker.
    let raw = String::from_utf8(output.stdout).expect("the oracle writes UTF-8");
    let text = normalize(&raw);
    let mut observed: Vec<(String, String)> = Vec::new();
    for entry in text.trim_end_matches(';').split(';') {
        let entry = entry.strip_suffix('@').unwrap_or(entry);
        let (name, value) = entry
            .split_once('@')
            .unwrap_or_else(|| panic!("malformed oracle line: {entry:?}"));
        observed.push((name.to_string(), value.to_string()));
    }

    let expected: Vec<(String, String)> = ORACLE
        .iter()
        .map(|(name, value)| ((*name).to_string(), normalize(value)))
        .collect();
    assert_eq!(observed, expected, "the live oracle disagrees with the pinned facts");
    println!("ORACLE: records:get_definition/2 agrees with the pinned definitions");
}

/// A fresh, unique directory under the system temp dir for the oracle cross-check.
fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("beam_loader_{tag}_{}_{nanos}", std::process::id()))
}

// ---------------------------------------------------------------------------
// Negative cases — a malformed `Recs` chunk is a typed error, never a panic
// ---------------------------------------------------------------------------

/// Appends one `FOR1`/`BEAM` chunk (id, size, payload, zero padding).
fn push_chunk(body: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    body.extend_from_slice(id);
    body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    body.extend_from_slice(payload);
    body.extend(std::iter::repeat(0u8).take((4 - payload.len() % 4) % 4));
}

/// Wraps `body` in a `FOR1`/`BEAM` container.
fn wrap(body: Vec<u8>) -> Vec<u8> {
    let mut image = Vec::new();
    image.extend_from_slice(b"FOR1");
    image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    image.extend_from_slice(b"BEAM");
    image.extend_from_slice(&body);
    image
}

/// A minimal, otherwise-valid module image carrying `recs` as its `Recs` payload.
///
/// The atom table holds `m` (the module name), `false`, `true`, `rec` and `f1`, so a
/// well-formed record entry can name them. The `Code` chunk holds one trivial
/// function — `label 1; func_info m:m/0; label 2; return; int_code_end` — which is
/// **required**: `lower::group_functions` rejects a stream with no `func_info` (the C
/// reader groups functions the same way, so a `.beam` without one is malformed). With
/// the function in place, only the `Recs` payload varies between the control image and
/// the malformed ones below, so each negative case fails in the `Recs` parser rather
/// than for a structural reason.
fn image_with_recs(recs: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();

    let mut atoms = Vec::new();
    atoms.extend_from_slice(&5u32.to_be_bytes());
    for name in ["m", "false", "true", "rec", "f1"] {
        atoms.push(name.len() as u8);
        atoms.extend_from_slice(name.as_bytes());
    }
    push_chunk(&mut body, b"AtU8", &atoms);

    let mut code = Vec::new();
    code.extend_from_slice(&16u32.to_be_bytes()); // head_size
    code.extend_from_slice(&0u32.to_be_bytes()); // format_number
    code.extend_from_slice(&181u32.to_be_bytes()); // max_opcode
    // `label_count` 3: the stream defines labels 1 and 2, and `lower::build_module`
    // rejects a label definition outside `0 < label < label_count`
    // (`jit/asm_load.c:512-521`).
    code.extend_from_slice(&3u32.to_be_bytes()); // label_count
    code.extend_from_slice(&1u32.to_be_bytes()); // function_count
    // `label 1` — opcode 1 with the `u,1` label operand.
    code.extend_from_slice(&[1, 0x10]);
    // `func_info m m 0` — opcode 2 with `{a,1}` (module `m`), `{a,1}` (function `m`)
    // and `{u,0}` (arity 0).
    code.extend_from_slice(&[2, 0x12, 0x12, 0x00]);
    // `label 2` — the function's entry label (it must follow `func_info`).
    code.extend_from_slice(&[1, 0x20]);
    // `return` (opcode 19) then the `int_code_end` (opcode 3) terminator.
    code.extend_from_slice(&[19]);
    code.push(3); // `int_code_end`
    push_chunk(&mut body, b"Code", &code);

    push_chunk(&mut body, b"Recs", recs);
    wrap(body)
}

/// A `Recs` header (`version 0`, `record_count`, `total_field_count`).
fn recs_header(record_count: i32, total_field_count: i32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&record_count.to_be_bytes());
    out.extend_from_slice(&total_field_count.to_be_bytes());
    out
}

/// Asserts the whole pipeline rejects `image` with `BeamError::BadArg`.
fn assert_rejected(image: &[u8], what: &str) {
    match load(image) {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected BeamError::BadArg, got {other:?}"),
    }
}

/// The control: a well-formed `Recs` payload loads, so the negative cases below are
/// not passing for a structural reason.
#[test]
fn a_hand_built_recs_chunk_loads() {
    let mut recs = recs_header(1, 2);
    recs.push(5); // `call_last` (the reused record-entry opcode)
    recs.extend_from_slice(&[0x42]); // atom 4 = `rec`
    recs.extend_from_slice(&[0x32]); // atom 3 = `true`
    recs.extend_from_slice(&[0x17, 0x20]); // `z,1` list, count 2 (one pair)
    recs.extend_from_slice(&[0x52]); // atom 5 = `f1`
    recs.extend_from_slice(&[0x00]); // `u,0` — no default

    let module = load(&image_with_recs(&recs)).expect("the control image must load");

    // The image is genuinely well-formed, not merely "not rejected": it carries the
    // `m:m/0` function every `func_info`-grouped `.beam` needs.
    assert_eq!(module.atom_name(module.name), Some("m"));
    assert_eq!(module.functions.len(), 1);
    assert_eq!(module.atom_name(module.functions[0].name), Some("m"));
    assert_eq!(module.functions[0].arity, 0);

    assert_eq!(module.records().len(), 1);
    let record = &module.records()[0];
    assert_eq!(module.atom_name(record.name), Some("rec"));
    assert!(record.is_exported);
    assert_eq!(declared_names(&module, record), ["f1"]);
    assert_eq!(record.fields[0].default, None);
}

#[test]
fn a_wrong_opcode_byte_is_rejected() {
    let mut recs = recs_header(1, 0);
    recs.push(4); // `call`, not `call_last`
    recs.extend_from_slice(&[0x42, 0x32, 0x17, 0x00]);
    assert_rejected(
        &image_with_recs(&recs),
        "a `Recs` entry that is not a `call_last` instruction",
    );
}

#[test]
fn an_odd_field_list_count_is_rejected() {
    let mut recs = recs_header(1, 1);
    recs.push(5);
    recs.extend_from_slice(&[0x42, 0x32]);
    recs.extend_from_slice(&[0x17, 0x10]); // `z,1` list, count 1 (odd)
    recs.extend_from_slice(&[0x52, 0x00]);
    assert_rejected(
        &image_with_recs(&recs),
        "an odd `(name, default)` list length",
    );
}

#[test]
fn a_non_atom_record_name_is_rejected() {
    let mut recs = recs_header(1, 0);
    recs.push(5);
    recs.extend_from_slice(&[0x11]); // `i,1` — an integer, not an atom
    recs.extend_from_slice(&[0x32, 0x17, 0x00]);
    assert_rejected(&image_with_recs(&recs), "a non-atom record name");
}

#[test]
fn an_export_flag_that_is_not_true_or_false_is_rejected() {
    let mut recs = recs_header(1, 0);
    recs.push(5);
    recs.extend_from_slice(&[0x42]); // atom 4 = `rec`
    recs.extend_from_slice(&[0x42]); // atom 4 = `rec` — neither `true` nor `false`
    recs.extend_from_slice(&[0x17, 0x00]);
    assert_rejected(
        &image_with_recs(&recs),
        "an export flag that is not `true`/`false`",
    );
}

#[test]
fn a_truncated_payload_is_rejected() {
    // The header declares one record but the payload stops right after the opcode.
    let mut recs = recs_header(1, 1);
    recs.push(5);
    assert_rejected(&image_with_recs(&recs), "a truncated record entry");

    // A payload shorter than its own header is rejected too.
    assert_rejected(&image_with_recs(&[0, 0]), "a truncated `Recs` header");

    // … and an empty payload.
    assert_rejected(&image_with_recs(&[]), "an empty `Recs` payload");
}

#[test]
fn a_record_count_larger_than_the_entries_is_rejected() {
    let mut recs = recs_header(2, 0);
    recs.push(5);
    recs.extend_from_slice(&[0x42, 0x32, 0x17, 0x00]); // the one real entry
    assert_rejected(
        &image_with_recs(&recs),
        "a record count larger than the payload's entries",
    );
}

#[test]
fn a_negative_record_count_is_rejected() {
    assert_rejected(
        &image_with_recs(&recs_header(-1, 0)),
        "a negative record count",
    );
}

#[test]
fn a_non_zero_version_yields_no_records() {
    // Mirror the C: a non-zero `Recs` version means "chunk-absent", *not* an error.
    let mut recs = recs_header(1, 1);
    recs[3] = 1; // version = 1
    recs.push(5);
    recs.extend_from_slice(&[0x42, 0x32, 0x17, 0x00]);

    let module = load(&image_with_recs(&recs)).expect("a versioned chunk is not fatal");
    assert!(module.records().is_empty());
}
