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

//! The `OffHeapBinaries` seam's **producer** half — the D2
//! `binary:referenced_byte_size/1` gap.
//!
//! With no host installed, this crate's whole-binary producers (`binary:copy/1,2`,
//! `binary:list_to_bin/1`, `erlang:iolist_to_binary/1`'s list form,
//! `erlang:split_binary/2`) materialize their result as an on-heap `ErlHeapBits`
//! however large it is, so a `binary:part/2,3` of it keeps the copy arm and
//! `binary:referenced_byte_size/1` answers the part's own size (pinned in
//! `binary_copy_cost.rs`). With a host installed they place a result above
//! `ERL_ONHEAP_BINARY_LIMIT` (64 bytes) off-heap through the host's
//! `alloc_refc_payload`, wrap it in the `[BinRef][ErlSubBits]` pair, and a later
//! `binary:part/2,3` window shares the parent payload — so the observable BIFs become
//! the oracle's.
//!
//! The rows below drive the registry directly with a mock host (a `Vec<RefcBinary>`
//! standing in for `beam_rt`'s off-heap store), plus a compiled-module cross-check
//! against the system oracle for the copy-based shapes.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_alloc::{Heap, RefcBinary};
use beam_bifs::{default_registry, BifContext, BifRegistry, OffHeapBinaries};
use beam_codec::term as codec_term;
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{make_small, signed_value, AtomTable, Eterm};

/// A host that retains every payload it mints — the production `beam_rt` off-heap
/// store, reduced to a `Vec<RefcBinary>`.
struct RefcHost {
    payloads: Vec<RefcBinary>,
}
impl RefcHost {
    fn new() -> RefcHost {
        RefcHost {
            payloads: Vec::new(),
        }
    }
}
impl OffHeapBinaries for RefcHost {
    fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut u8> {
        let payload = RefcBinary::new(bytes)?;
        let base = payload.data_ptr();
        // The handle joins the store before the pointer escapes, so the payload the
        // term names stays alive for as long as the fixture does.
        self.payloads.push(payload);
        Ok(base)
    }
}

/// A registry + heap + atom table with the [`OffHeapBinaries`] seam installed.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
    host: RefcHost,
}
impl Fixture {
    fn new() -> Fixture {
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 20).expect("test heap"),
            atoms: AtomTable::new(),
            host: RefcHost::new(),
        }
    }
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0))
            .with_off_heap(&mut self.host);
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }
    fn ok(&mut self, module: &str, function: &str, args: &[Eterm]) -> Eterm {
        self.call(module, function, args)
            .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
    }
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        codec_term::make_binary(&mut self.heap, bytes).expect("the binary fits")
    }
    fn list(&mut self, items: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        ctx.list(items).expect("the list fits")
    }
}

/// The bytes of a byte-aligned binary result.
fn bytes_of(term: Eterm) -> Vec<u8> {
    let (bytes, offset, size_bits) = codec_term::bitstring_bytes(term).expect("a bitstring result");
    assert_eq!(offset % 8, 0, "the result must start on a byte boundary");
    assert_eq!(size_bits % 8, 0, "the result must be a whole number of bytes");
    bytes[offset / 8..offset / 8 + size_bits / 8].to_vec()
}
/// `binary:referenced_byte_size/1` through the real registry.
fn referenced(fx: &mut Fixture, term: Eterm) -> i64 {
    signed_value(fx.ok("binary", "referenced_byte_size", &[term]))
}
/// `erlang:byte_size/1` through the real registry.
fn byte_size(fx: &mut Fixture, term: Eterm) -> i64 {
    signed_value(fx.ok("erlang", "byte_size", &[term]))
}

#[test]
fn binary_copy_above_the_limit_mints_an_off_heap_payload() {
    let mut fx = Fixture::new();
    let one = fx.binary(&[1]);
    let copy = fx.ok("binary", "copy", &[one, make_small(1000)]);
    // The whole result's own width is the copy's byte size.
    assert_eq!(bytes_of(copy).len(), 1000, "the copy's bytes");
    assert_eq!(referenced(&mut fx, copy), 1000, "a 1000-byte copy reports 1000");
    // The representation changed from on-heap to off-heap: a 510-byte part windows the
    // 1000-byte payload, so it reports the parent's 1000, not its own 510.
    let part = fx.ok("binary", "part", &[copy, make_small(1), make_small(510)]);
    assert_eq!(bytes_of(part), vec![1u8; 510], "the part's bytes");
    assert_eq!(byte_size(&mut fx, part), 510, "the part's own byte size");
    assert_eq!(
        referenced(&mut fx, part),
        1000,
        "the part shares the 1000-byte payload"
    );
    // Exactly one payload was minted — the copy's; the part's window is a new on-heap
    // term over the same payload, not a second payload.
    assert_eq!(fx.host.payloads.len(), 1, "one payload for the whole copy");
}

#[test]
fn binary_copy_at_or_below_the_limit_stays_on_heap_and_mints_nothing() {
    let mut fx = Fixture::new();
    let one = fx.binary(&[1]);
    for n in [1i64, 63, 64] {
        let copy = fx.ok("binary", "copy", &[one, make_small(n)]);
        assert_eq!(referenced(&mut fx, copy), n, "a {n}-byte copy reports {n}");
    }
    // None of these crosses the 64-byte arm, so the seam was never consulted.
    assert_eq!(fx.host.payloads.len(), 0, "the ≤64-byte arm mints no payload");
}

#[test]
fn binary_part_of_a_1024_byte_copy_shares_the_payload() {
    let mut fx = Fixture::new();
    let ff = fx.binary(&[255]);
    let copy = fx.ok("binary", "copy", &[ff, make_small(1024)]);
    assert_eq!(referenced(&mut fx, copy), 1024, "the copy reports 1024");
    let part = fx.ok("binary", "part", &[copy, make_small(1), make_small(510)]);
    assert_eq!(byte_size(&mut fx, part), 510);
    assert_eq!(
        referenced(&mut fx, part),
        1024,
        "the 510-byte part references the 1024-byte parent"
    );
    assert_eq!(bytes_of(part), vec![255u8; 510], "the part's bytes");
}

#[test]
fn list_to_bin_and_iolist_to_binary_route_through_the_same_seam() {
    let mut fx = Fixture::new();
    let one = fx.binary(&[1]);
    let copy = fx.ok("binary", "copy", &[one, make_small(1000)]);
    // `binary:list_to_bin(binary_to_list(C))` flattens to a fresh 1000-byte binary,
    // off-heap above the limit.
    let listed = fx.ok("binary", "bin_to_list", &[copy]);
    let round = fx.ok("binary", "list_to_bin", &[listed]);
    assert_eq!(bytes_of(round), vec![1u8; 1000], "the round-tripped bytes");
    let part = fx.ok("binary", "part", &[round, make_small(1), make_small(510)]);
    assert_eq!(
        referenced(&mut fx, part),
        1000,
        "a part of the list_to_bin result shares its 1000-byte payload"
    );
    // `erlang:iolist_to_binary([C])` (the list form) flattens to a fresh off-heap
    // binary; the bare-binary form returns the argument as-is and mints nothing.
    let iolist = fx.list(&[copy]);
    let flat = fx.ok("erlang", "iolist_to_binary", &[iolist]);
    assert_eq!(bytes_of(flat), vec![1u8; 1000], "the flattened bytes");
    let part = fx.ok("binary", "part", &[flat, make_small(1), make_small(510)]);
    assert_eq!(
        referenced(&mut fx, part),
        1000,
        "a part of the iolist_to_binary result shares its 1000-byte payload"
    );
}

#[test]
fn split_binary_windows_an_off_heap_parent() {
    let mut fx = Fixture::new();
    let one = fx.binary(&[1]);
    let copy = fx.ok("binary", "copy", &[one, make_small(1000)]);
    let pair = fx.ok("erlang", "split_binary", &[copy, make_small(100)]);
    assert_eq!(codec_term::tuple_arity(pair).expect("a 2-tuple"), 2);
    let before = codec_term::tuple_element(pair, 0).expect("element 0");
    let after = codec_term::tuple_element(pair, 1).expect("element 1");
    assert_eq!(byte_size(&mut fx, before), 100, "the 100-byte head");
    assert_eq!(byte_size(&mut fx, after), 900, "the 900-byte tail");
    assert_eq!(
        referenced(&mut fx, before),
        1000,
        "the head is a window over the 1000-byte parent"
    );
    assert_eq!(
        referenced(&mut fx, after),
        1000,
        "the tail is a window over the 1000-byte parent"
    );
}

// ---------------------------------------------------------------------------
// The live oracle's copy-based rows, driven through a compiled module
// ---------------------------------------------------------------------------
//
// `-eval` cannot answer these rows: there C's `binary:copy/2` mints a *writable*
// buffer whose `orig_size` is the allocated width, not the apparent size. A compiled
// module's `binary:copy/2` payload is non-writable, so it answers the copy's own byte
// size — the route `binary_copy_cost.rs`'s `refcwin` matrix was measured on.

/// The fixture: the copy-based shapes the task's oracle figures pin.
const REFCPRODUCER_ERL: &str = r#"
-module(refcproducer).
-export([run/0]).
run() ->
    C = binary:copy(<<255>>, 1024),
    io:format("copy_ref=~p~n", [binary:referenced_byte_size(C)]),
    P = binary:part(C, 1, 510),
    io:format("part_size=~p~n", [byte_size(P)]),
    io:format("part_ref=~p~n", [binary:referenced_byte_size(P)]),
    M = binary:copy(<<1>>, 1000),
    io:format("copy1000_ref=~p~n", [binary:referenced_byte_size(M)]),
    halt().
"#;

/// Whether `path` is an executable file.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && std::fs::metadata(path)
                .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
/// `name` in the first `$PATH` directory holding an executable of that name.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}
/// The system `erl`/`erlc`, honouring `$ERTS_ORACLE`'s sibling first.
fn oracle_tool(name: &str) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let erl = PathBuf::from(value);
        if let Some(sibling) = erl.parent().map(|dir| dir.join(name)) {
            if is_executable(&sibling) {
                return Some(sibling);
            }
        }
    }
    on_path(name)
}
/// A `$TMPDIR` scratch directory holding the compiled `refcproducer` fixture.
struct Scratch {
    dir: PathBuf,
}
impl Scratch {
    fn build(erlc: &Path) -> Scratch {
        let dir = std::env::temp_dir()
            .join(format!("beam_bifs_refcproducer_{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        let source = dir.join("refcproducer.erl");
        std::fs::write(&source, REFCPRODUCER_ERL)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", source.display()));
        let output = Command::new(erlc)
            .arg("-o")
            .arg(&dir)
            .arg(&source)
            .output()
            .unwrap_or_else(|e| panic!("cannot run `{}`: {e}", erlc.display()));
        assert!(
            output.status.success(),
            "`{}` exited with {:?}\n--- stderr ---\n{}",
            erlc.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        Scratch { dir }
    }
    fn path(&self) -> &Path {
        &self.dir
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
/// Runs `refcproducer:run()` in the oracle, panicking on spawn failure or non-zero exit.
fn run_refcproducer(erl: &Path, dir: &Path) -> String {
    let output = Command::new(erl)
        .arg("-noshell")
        .arg("-pa")
        .arg(dir)
        .args(["-eval", "refcproducer:run()."])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle `{}`: {e}", erl.display()));
    assert!(
        output.status.success(),
        "the oracle `{}` exited with {:?}\n--- stderr ---\n{}",
        erl.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}
/// The `key=<integer>` value for `key` in the transcript.
fn oracle_int(out: &str, key: &str) -> i64 {
    let prefix = format!("{key}=");
    out.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the oracle output has no `{key}=` line:\n{out}"))
        .parse()
        .unwrap_or_else(|e| panic!("the oracle's `{key}` is not an integer: {e}\n{out}"))
}

#[test]
fn the_copy_based_shapes_agree_with_the_oracle_row_for_row() {
    let (Some(erl), Some(erlc)) = (oracle_tool("erl"), oracle_tool("erlc")) else {
        eprintln!("===============================================================");
        eprintln!("[beam_bifs referenced_byte_size] No usable `erl`/`erlc` oracle found.");
        eprintln!("The compiled-module copy matrix is UNVERIFIED — NOT a pass.");
        eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
        eprintln!("===============================================================");
        return;
    };
    let scratch = Scratch::build(&erlc);
    let out = run_refcproducer(&erl, scratch.path());

    // 1. The oracle's own figures, against the task's recorded rows.
    assert_eq!(oracle_int(&out, "copy_ref"), 1024);
    assert_eq!(oracle_int(&out, "part_size"), 510);
    assert_eq!(oracle_int(&out, "part_ref"), 1024);
    assert_eq!(oracle_int(&out, "copy1000_ref"), 1000);

    // 2. The same shapes through this crate's registry with the seam installed.
    let mut fx = Fixture::new();
    let ff = fx.binary(&[255]);
    let copy = fx.ok("binary", "copy", &[ff, make_small(1024)]);
    assert_eq!(referenced(&mut fx, copy), oracle_int(&out, "copy_ref"));
    let part = fx.ok("binary", "part", &[copy, make_small(1), make_small(510)]);
    assert_eq!(byte_size(&mut fx, part), oracle_int(&out, "part_size"));
    assert_eq!(referenced(&mut fx, part), oracle_int(&out, "part_ref"));
    let one = fx.binary(&[1]);
    let copy1000 = fx.ok("binary", "copy", &[one, make_small(1000)]);
    assert_eq!(referenced(&mut fx, copy1000), oracle_int(&out, "copy1000_ref"));
}
