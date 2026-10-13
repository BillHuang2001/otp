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
//! Oracle-pinned integration tests for the `http` packet family of
//! `erlang:decode_packet/3` (`src/packet/http.rs`), driven through the **public**
//! API only (`default_registry()` + `BifRegistry::call`).
//!
//! Every golden below carries the probe that pinned it against the system OTP
//! 29.1.1 / erts-17.1 oracle. The result terms are inspected with the registered
//! `erlang:tuple_size/1`, `erlang:element/2`, `erlang:byte_size/1`,
//! `binary:at/2`, `erlang:hd/1`, `erlang:tl/1`, `erlang:is_*/1` and
//! `erlang:list_to_binary/1` BIFs — there is no public term-inspection API.
//!
//! One live cross-check shells out to the system `erl` and **skips loudly** when
//! the oracle is absent (the `tests/os_bif.rs` pattern); every other test passes
//! without an oracle.
use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm, NIL};

/// The 52 canonical header names in `http_hdr_strings` order
/// (`packet_parser.c:68-122`): `Name` for a known field is the table atom, so
/// every one of them must be interned for the parser to resolve it.
const HEADER_NAMES: [&str; 52] = [
    "Cache-Control", "Connection", "Date", "Pragma", "Transfer-Encoding", "Upgrade", "Via",
    "Accept", "Accept-Charset", "Accept-Encoding", "Accept-Language", "Authorization", "From",
    "Host", "If-Modified-Since", "If-Match", "If-None-Match", "If-Range", "If-Unmodified-Since",
    "Max-Forwards", "Proxy-Authorization", "Range", "Referer", "User-Agent", "Age", "Location",
    "Proxy-Authenticate", "Public", "Retry-After", "Server", "Vary", "Warning",
    "Www-Authenticate", "Allow", "Content-Base", "Content-Encoding", "Content-Language",
    "Content-Length", "Content-Location", "Content-Md5", "Content-Range", "Content-Type", "Etag",
    "Expires", "Last-Modified", "Accept-Ranges", "Set-Cookie", "Set-Cookie2", "X-Forwarded-For",
    "Cookie", "Keep-Alive", "Proxy-Connection",
];

/// The 7 canonical method names in `http_meth_strings` order
/// (`packet_parser.c:125-134`).
const METHOD_NAMES: [&str; 7] = ["OPTIONS", "GET", "HEAD", "POST", "PUT", "DELETE", "TRACE"];

// ---------------------------------------------------------------------------
// A thin wrapper so the shape helpers read as methods
// ---------------------------------------------------------------------------

/// The registry, the per-call [`BifContext`] and the atom table, bundled so the
/// term-inspection helpers can be called as methods.
struct T<'a, 'h> {
    reg: &'a BifRegistry,
    ctx: &'a mut BifContext<'h>,
    atoms: &'a AtomTable,
}

impl<'a, 'h> T<'a, 'h> {
    /// Bundles the registry, the per-call context and the shared atom table.
    fn new(reg: &'a BifRegistry, ctx: &'a mut BifContext<'h>, atoms: &'a AtomTable) -> T<'a, 'h> {
        T { reg, ctx, atoms }
    }

    /// A seeded atom, or a panic (every emitted atom is listed in `seeded_atoms`).
    fn atom(&self, name: &str) -> Eterm {
        self.ctx.atom(name).expect("a seeded atom")
    }

    fn int(&self, value: i64) -> Eterm {
        self.ctx.int(value).expect("a small integer")
    }

    fn tuple(&mut self, elems: &[Eterm]) -> Eterm {
        self.ctx.tuple(elems).expect("the tuple fits")
    }

    fn list(&mut self, elems: &[Eterm]) -> Eterm {
        self.ctx.list(elems).expect("the list fits")
    }

    /// An `{Key, SmallInt}` option tuple.
    fn option(&mut self, key: &str, value: i64) -> Eterm {
        let key = self.atom(key);
        let value = self.int(value);
        self.tuple(&[key, value])
    }

    /// `erlang:decode_packet(Type, Bin, Opts)` for a binary built from `bytes`.
    fn try_decode(&mut self, ty: &str, bytes: &[u8], opts: Eterm) -> Result<Eterm> {
        let ty = self.atom(ty);
        let bin = self.bin_term(bytes);
        self.reg.call("erlang", "decode_packet", 3, self.ctx, &[ty, bin, opts])
    }

    fn decode(&mut self, ty: &str, bytes: &[u8], opts: Eterm) -> Eterm {
        self.try_decode(ty, bytes, opts).expect("decode_packet")
    }

    /// A binary term, built by the registered `erlang:list_to_binary/1`.
    fn bin_term(&mut self, bytes: &[u8]) -> Eterm {
        let mut elems = Vec::new();
        for byte in bytes {
            elems.push(self.int(i64::from(*byte)));
        }
        let list = self.list(&elems);
        self.reg.call("erlang", "list_to_binary", 1, self.ctx, &[list]).expect("list_to_binary")
    }

    fn call(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Eterm {
        self.reg.call(module, function, arity, self.ctx, args).expect("the BIF succeeded")
    }

    /// `erlang:tuple_size/1`.
    fn size(&mut self, term: Eterm) -> usize {
        signed_value(self.call("erlang", "tuple_size", 1, &[term])) as usize
    }

    /// `erlang:element/2` (1-based).
    fn el(&mut self, index: i64, term: Eterm) -> Eterm {
        let index = self.int(index);
        self.call("erlang", "element", 2, &[index, term])
    }

    /// The bytes of a binary term, read back one byte at a time.
    fn bin_bytes(&mut self, term: Eterm) -> Vec<u8> {
        let size = signed_value(self.call("erlang", "byte_size", 1, &[term]));
        let mut out = Vec::new();
        for i in 0..size {
            let index = self.int(i);
            let byte = self.call("binary", "at", 2, &[term, index]);
            out.push(signed_value(byte) as u8);
        }
        out
    }

    /// The small integers of a proper list, via `erlang:hd/1`/`erlang:tl/1`.
    fn ints(&mut self, term: Eterm) -> Vec<i64> {
        let mut out = Vec::new();
        let mut cur = term;
        while cur != NIL {
            out.push(signed_value(self.call("erlang", "hd", 1, &[cur])));
            cur = self.call("erlang", "tl", 1, &[cur]);
        }
        out
    }

    /// Whether the registered `erlang:is_<name>/1` test answered `true`.
    fn is(&mut self, name: &str, term: Eterm) -> bool {
        let answer = self.call("erlang", name, 1, &[term]);
        self.name(answer) == "true"
    }

    /// The bytes of an emitted string, whichever representation it uses: the
    /// latin1 list of small ints or a `*_bin` byte-aligned binary.
    fn bytes(&mut self, term: Eterm) -> Vec<u8> {
        if self.is("is_binary", term) {
            self.bin_bytes(term)
        } else {
            self.ints(term).into_iter().map(|i| i as u8).collect()
        }
    }

    /// Asserts `term` is a string in the shape `as_binary` demands, and returns
    /// its bytes.
    fn string(&mut self, term: Eterm, as_binary: bool) -> Vec<u8> {
        assert_eq!(
            self.is("is_binary", term),
            as_binary,
            "the string representation must be {}",
            if as_binary { "a binary" } else { "a list" }
        );
        self.bytes(term)
    }

    /// Splits an `{ok, Body, Rest}` result into its body term and rest bytes.
    fn ok(&mut self, out: Eterm) -> (Eterm, Vec<u8>) {
        assert_eq!(self.size(out), 3, "{{ok,_,_}} is a 3-tuple");
        let ok = self.el(1, out);
        assert_eq!(self.name(ok), "ok");
        let body = self.el(2, out);
        let rest_term = self.el(3, out);
        let rest = self.bin_bytes(rest_term);
        (body, rest)
    }

    /// Asserts `out` is `{more, undefined}`.
    fn assert_more_undefined(&mut self, out: Eterm) {
        assert_eq!(self.size(out), 2);
        let tag = self.el(1, out);
        let reason = self.el(2, out);
        assert_eq!(self.name(tag), "more");
        assert_eq!(self.name(reason), "undefined");
    }

    /// Asserts `out` is `{error, invalid}`.
    fn assert_invalid(&mut self, out: Eterm) {
        assert_eq!(self.size(out), 2);
        let tag = self.el(1, out);
        let reason = self.el(2, out);
        assert_eq!(self.name(tag), "error");
        assert_eq!(self.name(reason), "invalid");
    }

    /// The name of an atom term, as the shared atom table spells it.
    fn name(&self, term: Eterm) -> String {
        self.atoms.name(atom_val(term)).expect("an interned atom name").to_string()
    }

    /// The name of `term` when it is an atom.
    fn atom_name_of(&mut self, term: Eterm) -> Option<String> {
        if self.is("is_atom", term) {
            Some(self.name(term))
        } else {
            None
        }
    }

    /// The small-integer value of `term`.
    fn small(term: Eterm) -> i64 {
        signed_value(term)
    }

    /// Decodes one request line with the `http` type (the `_bin` variants are
    /// covered by `the_bin_variants_emit_binaries_for_every_string`).
    fn request(&mut self, bytes: &[u8]) -> Request {
        let out = self.decode("http", bytes, NIL);
        let (body, rest) = self.ok(out);
        assert_eq!(self.size(body), 4, "{{http_request,M,U,V}}");
        let tag = self.el(1, body);
        assert_eq!(self.name(tag), "http_request");
        let method = self.el(2, body);
        let uri = self.el(3, body);
        let version = self.el(4, body);
        assert_eq!(self.size(version), 2, "{{Major,Minor}}");

        let kind = self.uri_kind(uri);
        let (https, host, port, text, scheme) = match kind {
            "absoluteURI" => {
                assert_eq!(self.size(uri), 5, "{{absoluteURI,S,H,P,Path}}");
                let scheme_term = self.el(2, uri);
                let https = match self.name(scheme_term).as_str() {
                    "http" => false,
                    "https" => true,
                    other => panic!("unknown absoluteURI scheme {other}"),
                };
                let host_term = self.el(3, uri);
                let path_term = self.el(5, uri);
                let port_term = self.el(4, uri);
                let port = if self.atom_name_of(port_term).is_some() {
                    assert_eq!(self.name(port_term), "undefined");
                    None
                } else {
                    Some(Self::small(port_term))
                };
                let host = self.bytes(host_term);
                let path = self.bytes(path_term);
                (https, host, port, path, Vec::new())
            }
            "abs_path" => {
                assert_eq!(self.size(uri), 2, "{{abs_path,S}}");
                let path_term = self.el(2, uri);
                let path = self.bytes(path_term);
                (false, Vec::new(), None, path, Vec::new())
            }
            "scheme" => {
                assert_eq!(self.size(uri), 3, "{{scheme,S1,S2}}");
                let s1 = self.el(2, uri);
                let s2 = self.el(3, uri);
                let (s1, s2) = (self.bytes(s1), self.bytes(s2));
                (false, Vec::new(), None, s2, s1)
            }
            "star" => (false, Vec::new(), None, b"*".to_vec(), Vec::new()),
            "string" => {
                let text = self.bytes(uri);
                (false, Vec::new(), None, text, Vec::new())
            }
            other => panic!("unknown URI kind {other}"),
        };
        let major = self.el(1, version);
        let minor = self.el(2, version);
        Request {
            kind,
            https,
            host,
            port,
            text,
            scheme,
            method,
            major: Self::small(major),
            minor: Self::small(minor),
            rest,
        }
    }

    /// The URI's tag, as the emitted term spells it.
    fn uri_kind(&mut self, uri: Eterm) -> &'static str {
        if self.atom_name_of(uri).is_some() {
            // `URI_STAR` is the only bare-atom form.
            assert_eq!(self.name(uri), "*");
            return "star";
        }
        if !self.is("is_tuple", uri) {
            // `URI_STRING`: the target is reported verbatim.
            return "string";
        }
        let tag = self.el(1, uri);
        match self.name(tag).as_str() {
            "abs_path" => "abs_path",
            "absoluteURI" => "absoluteURI",
            "scheme" => "scheme",
            other => panic!("unknown URI tag {other}"),
        }
    }

    /// Decodes one header line with `ty` (`httph`/`httph_bin`).
    fn header(&mut self, ty: &str, bytes: &[u8]) -> Header {
        let as_binary = ty.ends_with("_bin");
        let out = self.decode(ty, bytes, NIL);
        let (body, rest) = self.ok(out);
        assert_eq!(self.size(body), 5, "{{http_header,B,N,O,V}}");
        let tag = self.el(1, body);
        assert_eq!(self.name(tag), "http_header");
        let bit = self.el(2, body);
        let name = self.el(3, body);
        let oname_term = self.el(4, body);
        let oname = self.string(oname_term, as_binary);
        let value_term = self.el(5, body);
        let value = self.string(value_term, as_binary);
        Header { bit: Self::small(bit), name, oname, value, rest }
    }
}

/// A parsed `{http_request, Method, Uri, {Major,Minor}}` with the URI's parts
/// pulled out. `kind` is `"star"`, `"abs_path"`, `"absoluteURI"`, `"scheme"` or
/// `"string"`.
struct Request {
    kind: &'static str,
    https: bool,
    /// The host bytes of an `absoluteURI` (empty otherwise).
    host: Vec<u8>,
    /// The port: `None` for the atom `undefined`.
    port: Option<i64>,
    /// The URI payload: the path/params/query of an `absoluteURI`, the path of
    /// an `abs_path`, the trailing chars of a `scheme`, or the whole target of a
    /// bare string / the `*` atom.
    text: Vec<u8>,
    /// The scheme string of a `{scheme, S1, S2}` URI (empty otherwise).
    scheme: Vec<u8>,
    /// The method term (an atom when the table matched, else a string).
    method: Eterm,
    major: i64,
    minor: i64,
    rest: Vec<u8>,
}

/// A parsed `{http_header, Bit, Name, Oname, Value}`.
struct Header {
    bit: i64,
    name: Eterm,
    oname: Vec<u8>,
    value: Vec<u8>,
    rest: Vec<u8>,
}

/// Every atom the http family can emit, or resolve by name, must be seeded.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    let mut names: Vec<&str> = vec![
        // the result tags + the type and option atoms `mod.rs` resolves
        "ok",
        "error",
        "more",
        "undefined",
        "invalid",
        "true",
        "false",
        "http",
        "httph",
        "http_bin",
        "httph_bin",
        "packet_size",
        "line_length",
        "line_delimiter",
        // the http-family result atoms
        "http_request",
        "http_response",
        "http_header",
        "http_eoh",
        "http_error",
        "abs_path",
        "absoluteURI",
        "scheme",
        "https",
        "*",
    ];
    names.extend(METHOD_NAMES);
    names.extend(HEADER_NAMES);
    atoms.seed(names);
    atoms
}

/// Runs `body` with a fresh registry, heap, atom table and [`T`].
///
/// The heap, atom table and registry are leaked so the context that borrows
/// them can outlive this frame; a test process exits right afterwards.
fn with_context(body: impl FnOnce(&mut T<'static, 'static>)) {
    let reg: &'static BifRegistry = Box::leak(Box::new(default_registry()));
    let atoms: &'static AtomTable = Box::leak(Box::new(seeded_atoms()));
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(1 << 18).expect("test heap")));
    let ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
    let mut t = T::new(reg, Box::leak(Box::new(ctx)), atoms);
    body(&mut t);
}

// ---------------------------------------------------------------------------
// Request line
// ---------------------------------------------------------------------------

#[test]
fn a_request_line_carries_the_method_uri_and_version() {
    // ORACLE: decode_packet(http, <<"GET /index.html HTTP/1.1\r\nHost: x\r\n\r\nrest">>, [])
    //   => {ok,{http_request,'GET',{abs_path,"/index.html"},{1,1}},
    //          <<"Host: x\r\n\r\nrest">>}
    with_context(|t| {
        let r = t.request(b"GET /index.html HTTP/1.1\r\nHost: x\r\n\r\nrest");
        assert_eq!(r.rest, b"Host: x\r\n\r\nrest", "only the request line is one packet");
        assert_eq!(t.name(r.method), "GET");
        assert_eq!(r.kind, "abs_path");
        assert_eq!(r.text, b"/index.html");
        assert_eq!((r.major, r.minor), (1, 1));
    });
}

#[test]
fn the_bin_variants_emit_binaries_for_every_string() {
    // ORACLE: decode_packet(http_bin, <<"GET /index.html HTTP/1.1\r\nHost: x\r\n\r\nrest">>, [])
    //   => {ok,{http_request,'GET',{abs_path,<<"/index.html">>},{1,1}},
    //          <<"Host: x\r\n\r\nrest">>}
    with_context(|t| {
        let out = t.decode("http_bin", b"GET /index.html HTTP/1.1\r\nHost: x\r\n\r\nrest", NIL);
        let (body, rest) = t.ok(out);
        assert_eq!(rest, b"Host: x\r\n\r\nrest");
        let tag = t.el(1, body);
        assert_eq!(t.name(tag), "http_request");
        // The method stays the table atom even in binary mode ...
        let method = t.el(2, body);
        assert_eq!(t.name(method), "GET");
        let uri = t.el(3, body);
        let uri_tag = t.el(1, uri);
        assert_eq!(t.name(uri_tag), "abs_path");
        let path = t.el(2, uri);
        assert_eq!(t.string(path, true), b"/index.html".to_vec());
        let version = t.el(4, body);
        let major = t.el(1, version);
        let minor = t.el(2, version);
        assert_eq!((T::small(major), T::small(minor)), (1, 1));
    });
}

#[test]
fn a_request_without_a_version_is_0_9() {
    with_context(|t| {
        // ORACLE: decode_packet(http, <<"GET /\r\n">>, [])
        //   => {ok,{http_request,'GET',{abs_path,"/"},{0,9}},<<>>}
        let r = t.request(b"GET /\r\n");
        assert_eq!(r.rest, b"");
        assert_eq!(t.name(r.method), "GET");
        assert_eq!(r.kind, "abs_path");
        assert_eq!(r.text, b"/");
        assert_eq!((r.major, r.minor), (0, 9));

        // ORACLE: decode_packet(http, <<"GET / HTTP/1.1\r\n">>, [])
        //   => {ok,{http_request,'GET',{abs_path,"/"},{1,1}},<<>>}
        let r = t.request(b"GET / HTTP/1.1\r\n");
        assert_eq!(r.rest, b"");
        assert_eq!(r.text, b"/");
        assert_eq!((r.major, r.minor), (1, 1));
    });
}

#[test]
fn the_method_stops_at_a_tspecial_and_falls_back_to_bytes() {
    with_context(|t| {
        // A tab is a tspecial, so `GET\t/x` still names the table's `GET`.
        // ORACLE: decode_packet(http, <<"GET\t/x HTTP/1.1\r\n">>, [])
        //   => {ok,{http_request,'GET',{abs_path,"/x"},{1,1}},<<>>}
        let r = t.request(b"GET\t/x HTTP/1.1\r\n");
        assert_eq!(t.name(r.method), "GET");
        assert_eq!(r.text, b"/x");

        // A method that is not one of the seven is emitted as its raw bytes.
        // ORACLE: decode_packet(http, <<"gEt /x HTTP/1.1\r\n">>, [])
        //   => {ok,{http_request,[103,69,116],{abs_path,"/x"},{1,1}},<<>>}
        let r = t.request(b"gEt /x HTTP/1.1\r\n");
        assert_eq!(t.ints(r.method), vec![103, 69, 116]);
        assert_eq!(r.text, b"/x");

        // Trailing bytes after the version are ignored.
        // ORACLE: decode_packet(http, <<"GET /x HTTP/1.1 extra\r\n">>, [])
        //   => {ok,{http_request,'GET',{abs_path,"/x"},{1,1}},<<>>}
        let r = t.request(b"GET /x HTTP/1.1 extra\r\n");
        assert_eq!((r.major, r.minor), (1, 1));
        assert_eq!(r.rest, b"");

        // Repeated white space between the tokens is skipped.
        // ORACLE: decode_packet(http, <<"GET  /x HTTP/1.1\r\n">>, []) => ... {abs_path,"/x"}
        let r = t.request(b"GET  /x HTTP/1.1\r\n");
        assert_eq!(r.text, b"/x");
    });
}

#[test]
fn every_uri_shape_is_reachable() {
    with_context(|t| {
        // ORACLE: `OPTIONS *` => 'OPTIONS' + the atom '*'
        let r = t.request(b"OPTIONS * HTTP/1.1\r\n");
        assert_eq!(t.name(r.method), "OPTIONS");
        assert_eq!(r.kind, "star");
        assert_eq!(r.text, b"*");

        // ORACLE: `GET **` => the bare string "**" (only a *single* `*` is the atom)
        let r = t.request(b"GET ** HTTP/1.1\r\n");
        assert_eq!(r.kind, "string");
        assert_eq!(r.text, b"**");

        // ORACLE: a 1-byte target is always `abs_path`, even `.` or `:`
        for uri in [&b"."[..], b":", b"a"] {
            let line = [b"GET ".as_slice(), uri, b" HTTP/1.1\r\n"].concat();
            let r = t.request(&line);
            assert_eq!(r.kind, "abs_path", "uri {uri:?}");
            assert_eq!(r.text, uri, "uri {uri:?}");
        }

        // ORACLE: no `:` at all => the bare string
        let r = t.request(b"GET foo HTTP/1.1\r\n");
        assert_eq!(r.kind, "string");
        assert_eq!(r.text, b"foo");

        // ORACLE: `foo:bar` => {scheme,"foo","bar"}
        let r = t.request(b"GET foo:bar HTTP/1.1\r\n");
        assert_eq!(r.kind, "scheme");
        assert_eq!(r.scheme, b"foo");
        assert_eq!(r.text, b"bar");

        // ORACLE: `http:/x` is *not* the `http://` prefix => {scheme,"http","/x"}
        let r = t.request(b"GET http:/x HTTP/1.1\r\n");
        assert_eq!(r.kind, "scheme");
        assert_eq!(r.scheme, b"http");
        assert_eq!(r.text, b"/x");
    });
}

#[test]
fn an_absolute_uri_reports_scheme_host_port_and_path() {
    with_context(|t| {
        // ORACLE: `http://host` => {absoluteURI,http,"host",undefined,"/"}
        let r = t.request(b"GET http://host HTTP/1.1\r\n");
        assert_eq!(r.kind, "absoluteURI");
        assert!(!r.https);
        assert_eq!(r.host, b"host");
        assert_eq!(r.port, None);
        assert_eq!(r.text, b"/");

        // ORACLE: `http://host/x` => path "/x"
        let r = t.request(b"GET http://host/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"host");
        assert_eq!(r.port, None);
        assert_eq!(r.text, b"/x");

        // ORACLE: `http://host:8080/x` => port 8080
        let r = t.request(b"GET http://host:8080/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"host");
        assert_eq!(r.port, Some(8080));
        assert_eq!(r.text, b"/x");

        // ORACLE: a port of `0` is "undefined" (the C's `port == 0` sentinel)
        let r = t.request(b"GET http://host:0/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"host");
        assert_eq!(r.port, None);

        // ORACLE: a non-numeric port leaves it undefined and the host intact
        let r = t.request(b"GET http://host:xy/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"host");
        assert_eq!(r.port, None);

        // ORACLE: an empty port (`h:`) is undefined too
        let r = t.request(b"GET http://h:/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"h");
        assert_eq!(r.port, None);

        // ORACLE: `https://HOST:443/x` HTTP/1.0 => https + the host *verbatim*
        let r = t.request(b"GET https://HOST:443/x HTTP/1.0\r\n");
        assert!(r.https);
        assert_eq!(r.host, b"HOST", "the host is never case-normalised");
        assert_eq!(r.port, Some(443));
        assert_eq!(r.text, b"/x");
        assert_eq!((r.major, r.minor), (1, 0));

        // ORACLE: the scheme prefix is case-insensitive but the emitted scheme
        // atom is lower case and the host keeps its case.
        let r = t.request(b"GET HTTP://EX.COM/a HTTP/1.1\r\n");
        assert!(!r.https);
        assert_eq!(r.host, b"EX.COM");
        assert_eq!(r.port, None);
        assert_eq!(r.text, b"/a");
    });
}

#[test]
fn an_ipv6_absolute_uri_keeps_its_brackets() {
    with_context(|t| {
        // ORACLE: `http://[::1]:4000/x` => host "[::1]", port 4000
        let r = t.request(b"GET http://[::1]:4000/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"[::1]");
        assert_eq!(r.port, Some(4000));
        assert_eq!(r.text, b"/x");

        // ORACLE: `http://[::1]/x` => no port
        let r = t.request(b"GET http://[::1]/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"[::1]");
        assert_eq!(r.port, None);

        // ORACLE: a bracketed host with a port on a longer address
        let r = t.request(b"GET http://[FEDC:BA98:7654:3210]:80/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"[FEDC:BA98:7654:3210]");
        assert_eq!(r.port, Some(80));

        // ORACLE: an empty bracketed port `[::1]:` => undefined
        let r = t.request(b"GET http://[::1]:/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"[::1]");
        assert_eq!(r.port, None);

        // ORACLE: bytes between `]` and `:port` keep the port undefined
        let r = t.request(b"GET http://[::1]abc:80/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"[::1]");
        assert_eq!(r.port, None);

        // ORACLE: the IPv6 port accumulator wraps like the C `int` too
        let r = t.request(b"GET http://[::1]:99999999999999/x HTTP/1.1\r\n");
        assert_eq!(r.host, b"[::1]");
        assert_eq!(r.port, Some(276447231));
    });
}

#[test]
fn the_numeric_accumulators_wrap_like_the_c_int() {
    // The C accumulates `major`/`minor`/port into an `int`, which wraps on
    // overflow; a checked accumulator would have made these a capability gap.
    with_context(|t| {
        // ORACLE: `HTTP/99999999999999999999.1` => {1661992959,1}
        let r = t.request(b"GET /x HTTP/99999999999999999999.1\r\n");
        assert_eq!((r.major, r.minor), (1661992959, 1));

        // ORACLE: `HTTP/1.99999999999999999999` => {1,1661992959}
        let r = t.request(b"GET /x HTTP/1.99999999999999999999\r\n");
        assert_eq!((r.major, r.minor), (1, 1661992959));

        // ORACLE: `http://h:99999999999999` => port 276447231
        let r = t.request(b"GET http://h:99999999999999/x HTTP/1.1\r\n");
        assert_eq!(r.port, Some(276447231));
    });
}

#[test]
fn a_status_line_carries_version_status_and_phrase() {
    with_context(|t| {
        // ORACLE: decode_packet(http, <<"HTTP/1.1 200 OK\r\n">>, [])
        //   => {ok,{http_response,{1,1},200,"OK"},<<>>}
        let out = t.decode("http", b"HTTP/1.1 200 OK\r\n", NIL);
        let (body, rest) = t.ok(out);
        assert_eq!(rest, b"");
        assert_eq!(t.size(body), 4, "{{http_response,V,Status,Phrase}}");
        let tag = t.el(1, body);
        assert_eq!(t.name(tag), "http_response");
        let version = t.el(2, body);
        let major = t.el(1, version);
        let minor = t.el(2, version);
        assert_eq!((T::small(major), T::small(minor)), (1, 1));
        let status = t.el(3, body);
        assert_eq!(T::small(status), 200);
        let phrase = t.el(4, body);
        assert_eq!(t.string(phrase, false), b"OK".to_vec());

        // ORACLE: `HTTP/1.1 404 Not Found` (a multi-word phrase) + the `_bin` form.
        for (name, wants_binary) in [("http", false), ("http_bin", true)] {
            let out = t.decode(name, b"HTTP/1.1 404 Not Found\r\n", NIL);
            let (body, rest) = t.ok(out);
            assert_eq!(rest, b"");
            let tag = t.el(1, body);
            assert_eq!(t.name(tag), "http_response");
            let status = t.el(3, body);
            assert_eq!(T::small(status), 404);
            let phrase = t.el(4, body);
            assert_eq!(t.string(phrase, wants_binary), b"Not Found".to_vec());
        }

        // ORACLE: `HTTP/1.1 ` (no status digits) => status 0, empty phrase
        //   => {ok,{http_response,{1,1},0,[]},<<>>}
        let out = t.decode("http", b"HTTP/1.1 \r\n", NIL);
        let (body, _) = t.ok(out);
        let status = t.el(3, body);
        assert_eq!(T::small(status), 0);
        let phrase = t.el(4, body);
        assert_eq!(t.ints(phrase), Vec::<i64>::new());
    });
}

// ---------------------------------------------------------------------------
// Malformed lines => {http_error, Line}
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_line_is_the_whole_packet_in_http_error() {
    // Each case is (type, input, expected {http_error, Line} bytes).
    let cases: &[(&str, &[u8], &[u8])] = &[
        // ORACLE: a header line is not a valid request line in request mode
        ("http", b"X-Abc: v\r\n", b"X-Abc: v\r\n"),
        ("http", b"GET \r\n", b"GET \r\n"),
        // ORACLE: a leading space makes the method empty
        ("http", b" GET /x HTTP/1.1\r\n", b" GET /x HTTP/1.1\r\n"),
        // ORACLE: `HTTP/1.1` with neither SP nor a status
        ("http", b"HTTP/1.1\r\n", b"HTTP/1.1\r\n"),
        // ORACLE: no white space between the status and the phrase
        ("http", b"HTTP/1.1 200OK\r\n", b"HTTP/1.1 200OK\r\n"),
        // ORACLE: the version must be exactly `HTTP/<digits>.<digits>`
        ("http", b"HTTP/2 200 OK\r\n", b"HTTP/2 200 OK\r\n"),
        ("http", b"HTTP/1. 200 OK\r\n", b"HTTP/1. 200 OK\r\n"),
        ("http", b"HTTP/.1 200 OK\r\n", b"HTTP/.1 200 OK\r\n"),
        // ORACLE: a non-digit status
        ("http", b"HTTP/1.1 abc\r\n", b"HTTP/1.1 abc\r\n"),
        // ORACLE: a bare NL / CRLF is a whole packet that is not a start line
        ("http", b"\n", b"\n"),
        ("http", b"\r\n", b"\r\n"),
        // ORACLE: a header name with no `:` (header mode)
        ("httph", b"Host\r\nz", b"Host\r\n"),
        // ORACLE: a space inside the header name
        ("httph", b"Ho st: x\r\nz", b"Ho st: x\r\n"),
    ];
    with_context(|t| {
        for (name, input, line) in cases {
            let out = t.decode(name, input, NIL);
            let (body, _) = t.ok(out);
            assert_eq!(t.size(body), 2, "{input:?}");
            let tag = t.el(1, body);
            assert_eq!(t.name(tag), "http_error", "{input:?}");
            let err_line = t.el(2, body);
            assert_eq!(t.bytes(err_line), *line, "{input:?}");
        }

        // ORACLE: the `_bin` form reports the error line as a binary
        // decode_packet(http_bin, <<"X-Abc: v\r\n">>, [])
        //   => {ok,{http_error,<<88,45,65,98,99,58,32,118,13,10>>},<<>>}
        let out = t.decode("http_bin", b"X-Abc: v\r\n", NIL);
        let (body, _) = t.ok(out);
        let tag = t.el(1, body);
        assert_eq!(t.name(tag), "http_error");
        let err_line = t.el(2, body);
        assert_eq!(t.string(err_line, true), b"X-Abc: v\r\n".to_vec());
    });
}

// ---------------------------------------------------------------------------
// Header lines
// ---------------------------------------------------------------------------

#[test]
fn a_known_header_field_reports_its_index_atom_and_original_spelling() {
    with_context(|t| {
        // ORACLE: decode_packet(httph, <<"Host: x\r\nrest">>, [])
        //   => {ok,{http_header,14,'Host',"Host","x"},<<"rest">>}
        let h = t.header("httph", b"Host: x\r\nrest");
        assert_eq!(h.bit, 14);
        assert_eq!(t.name(h.name), "Host");
        assert_eq!(h.oname, b"Host");
        assert_eq!(h.value, b"x");
        assert_eq!(h.rest, b"rest");

        // ORACLE: decode_packet(httph_bin, <<"Host: x\r\nrest">>, [])
        //   => {ok,{http_header,14,'Host',<<"Host">>,<<"x">>},<<"rest">>}
        let h = t.header("httph_bin", b"Host: x\r\nrest");
        assert_eq!(h.bit, 14);
        assert_eq!(t.name(h.name), "Host");
        assert_eq!(h.oname, b"Host");
        assert_eq!(h.value, b"x");
        assert_eq!(h.rest, b"rest");

        // ORACLE: the `Bit` is the 1-based index into the 52-entry table
        for (line, bit, name) in [
            (&b"Cache-Control: no-cache\r\nz"[..], 1, "Cache-Control"),
            (b"Www-Authenticate: X\r\nz", 33, "Www-Authenticate"),
            (b"Content-Length: 5\r\nz", 38, "Content-Length"),
            (b"Set-Cookie2: a\r\nz", 48, "Set-Cookie2"),
            (b"Keep-Alive: 1\r\nz", 51, "Keep-Alive"),
        ] {
            let h = t.header("httph", line);
            assert_eq!(h.bit, bit, "{line:?}");
            assert_eq!(t.name(h.name), name, "{line:?}");
        }
    });
}

#[test]
fn an_unknown_header_field_is_normalised_up_to_50_characters() {
    with_context(|t| {
        // ORACLE: an unknown name is `Bit = 0`, `Name` = the *normalised* bytes,
        // `Oname` = the *original* bytes.
        // decode_packet(httph, <<"X-Abc: v\r\nz">>, [])
        //   => {ok,{http_header,0,"X-Abc","X-Abc","v"},<<"z">>}
        let h = t.header("httph", b"X-Abc: v\r\nz");
        assert_eq!(h.bit, 0);
        assert_eq!(t.string(h.name, false), b"X-Abc".to_vec());
        assert_eq!(h.oname, b"X-Abc");
        assert_eq!(h.value, b"v");
        assert_eq!(h.rest, b"z");

        // ORACLE: a lower-case spelling normalises to the canonical name while
        // the `Oname` keeps the original case.
        // decode_packet(httph, <<"host: x\r\nz">>, [])
        //   => {ok,{http_header,14,'Host',"host","x"},<<"z">>}
        let h = t.header("httph", b"host: x\r\nz");
        assert_eq!(h.bit, 14);
        assert_eq!(t.name(h.name), "Host");
        assert_eq!(h.oname, b"host");

        // ORACLE: `cONTENT-lENGTH` normalises to `Content-Length` (index 37 => bit 38)
        let h = t.header("httph", b"cONTENT-lENGTH: 5\r\nz");
        assert_eq!(h.bit, 38);
        assert_eq!(t.name(h.name), "Content-Length");
        assert_eq!(h.oname, b"cONTENT-lENGTH");
        assert_eq!(h.value, b"5");

        // ORACLE: exactly 50 characters still normalises (first char up-cased)
        // and the `Oname` is the original 50 bytes.
        let name50 = format!("{}: v\r\nz", "a".repeat(50));
        let h = t.header("httph", name50.as_bytes());
        assert_eq!(h.bit, 0);
        let mut want = vec![b'A'];
        want.extend(vec![b'a'; 49]);
        assert_eq!(t.string(h.name, false), want);
        assert_eq!(h.oname, "a".repeat(50).into_bytes());

        // ORACLE: 51 characters exceed `HTTP_MAX_NAME_LEN`, so both `Name` and
        // `Oname` are the original bytes (`name_ptr = buf`, `name = NULL`).
        let name51 = format!("{}: v\r\nz", "a".repeat(51));
        let h = t.header("httph", name51.as_bytes());
        assert_eq!(h.bit, 0);
        assert_eq!(t.string(h.name, false), "a".repeat(51).into_bytes());
        assert_eq!(h.oname, "a".repeat(51).into_bytes());

        // ORACLE: an empty name (a line starting with `:`) is `Bit = 0` with
        // empty name/oname and the value after the colon.
        // decode_packet(httph, <<": x\r\nz">>, []) => {ok,{http_header,0,[],[],[120]},<<122>>}
        let h = t.header("httph", b": x\r\nz");
        assert_eq!(h.bit, 0);
        assert_eq!(t.ints(h.name), Vec::<i64>::new());
        assert_eq!(h.oname, Vec::<u8>::new());
        assert_eq!(h.value, b"x");

        // ORACLE: a `_bin` unknown name is a binary in the normalised spelling
        // decode_packet(httph_bin, <<"X-Abc: v\r\nz">>, [])
        //   => {ok,{http_header,0,<<88,45,65,98,99>>,<<88,45,65,98,99>>,<<118>>},<<122>>}
        let h = t.header("httph_bin", b"X-Abc: v\r\nz");
        assert_eq!(h.bit, 0);
        assert_eq!(t.string(h.name, true), b"X-Abc".to_vec());
        assert_eq!(h.oname, b"X-Abc");
        assert_eq!(h.value, b"v");
    });
}

#[test]
fn header_white_space_and_value_bytes_are_preserved() {
    with_context(|t| {
        // ORACLE: `Host:x` — no space after the colon still parses
        let h = t.header("httph", b"Host:x\r\nz");
        assert_eq!(h.bit, 14);
        assert_eq!(h.oname, b"Host");
        assert_eq!(h.value, b"x");

        // ORACLE: only the white space *after* the colon is skipped; trailing
        // bytes of the value are kept (`Host:  x  ` => "x  ").
        let h = t.header("httph", b"Host:  x  \r\nz");
        assert_eq!(h.value, b"x  ");

        // ORACLE: an empty value (`Host:`) and a lone space (`Host: `) both give "".
        for line in [&b"Host:\r\nz"[..], b"Host: \r\nz"] {
            let h = t.header("httph", line);
            assert_eq!(h.value, b"", "{line:?}");
        }

        // ORACLE: a bare NL terminator is accepted as well as CRLF
        // decode_packet(httph, <<"Host: x\nz">>, [])
        //   => {ok,{http_header,14,'Host',"Host","x"},<<"z">>}
        let h = t.header("httph", b"Host: x\nz");
        assert_eq!(h.bit, 14);
        assert_eq!(h.value, b"x");
        assert_eq!(h.rest, b"z");
    });
}

#[test]
fn a_continuation_line_joins_the_header_value() {
    with_context(|t| {
        // ORACLE: decode_packet(httph, <<"Host: x\r\n more\r\nz">>, [])
        //   => {ok,{http_header,14,'Host',"Host","x\r\n more"},<<"z">>}
        let h = t.header("httph", b"Host: x\r\n more\r\nz");
        assert_eq!(h.bit, 14);
        assert_eq!(h.value, b"x\r\n more");
        assert_eq!(h.rest, b"z");

        // ORACLE: a continuation line holding only a space contributes " \r\n"
        // decode_packet(httph, <<"Host: x\r\n \r\nz">>, []) => value "x\r\n "
        let h = t.header("httph", b"Host: x\r\n \r\nz");
        assert_eq!(h.value, b"x\r\n ");
        assert_eq!(h.rest, b"z");

        // ORACLE: a bare CRLF is never a continuation — it ends the headers, so
        // the field is complete and the rest starts at the blank line.
        // decode_packet(httph, <<"Host: x\r\n\r\nz">>, [])
        //   => {ok,{http_header,14,'Host',"Host","x"},<<"\r\nz">>}
        let h = t.header("httph", b"Host: x\r\n\r\nz");
        assert_eq!(h.bit, 14);
        assert_eq!(h.value, b"x");
        assert_eq!(h.rest, b"\r\nz");
    });
}

#[test]
fn a_blank_line_in_header_mode_is_http_eoh() {
    // ORACLE: decode_packet(httph, <<"\r\nrest">>, []) => {ok,http_eoh,<<"rest">>}
    with_context(|t| {
        for input in [&b"\r\nrest"[..], b"\nrest"] {
            for name in ["httph", "httph_bin"] {
                let out = t.decode(name, input, NIL);
                let (body, rest) = t.ok(out);
                assert_eq!(t.name(body), "http_eoh", "{name} {input:?}");
                assert_eq!(rest, b"rest", "{name} {input:?}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Length rule: {more, …} and the ceilings
// ---------------------------------------------------------------------------

#[test]
fn an_unterminated_line_needs_more_data() {
    // ORACLE: every one of these is `{more,undefined}` — the C's `more:` label
    // reached with a zero length (`erl_bif_port.c:1564-1569`).
    let cases: &[(&str, &[u8])] = &[
        ("http", b"GET /x HTTP/1.1"),
        ("http", b"GET /x HTTP/1.1\r"),
        ("http", b"GET /x"),
        ("http", b"GET /"),
        ("httph", b"Host: x\r\n"),
        ("httph", b"Host: x"),
        ("httph", b"X: y\r\n"),
        ("httph", b""),
        ("http", b""),
    ];
    with_context(|t| {
        for (name, input) in cases {
            let out = t.decode(name, input, NIL);
            t.assert_more_undefined(out);
        }
    });
}

#[test]
fn packet_size_and_line_length_are_hard_ceilings() {
    with_context(|t| {
        // A `packet_size` that covers the whole line is fine (the line is 9 bytes).
        // ORACLE: decode_packet(httph, <<"Host: x\r\nz">>, [{packet_size,9}])
        //   => {ok,{http_header,14,'Host',"Host","x"},<<"z">>}
        let opt = t.option("packet_size", 9);
        let opts = t.list(&[opt]);
        let out = t.decode("httph", b"Host: x\r\nz", opts);
        let (body, rest) = t.ok(out);
        let tag = t.el(1, body);
        assert_eq!(t.name(tag), "http_header");
        assert_eq!(rest, b"z");

        // One byte less is `{error,invalid}` (the C's `error:` label).
        // ORACLE: ... [{packet_size,8}] => {error,invalid}
        let opt = t.option("packet_size", 8);
        let opts = t.list(&[opt]);
        let out = t.decode("httph", b"Host: x\r\nz", opts);
        t.assert_invalid(out);

        // `line_length` is the fallback when `packet_size` is genuinely unset
        // (the C's `if (!max_plen) max_plen = trunc_len`).
        // ORACLE: [{line_length,5}] and [{packet_size,0},{line_length,5}]
        //   => both {error,invalid}
        for with_packet_size in [false, true] {
            let opts = if with_packet_size {
                let zero = t.option("packet_size", 0);
                let five = t.option("line_length", 5);
                t.list(&[zero, five])
            } else {
                let five = t.option("line_length", 5);
                t.list(&[five])
            };
            let out = t.decode("httph", b"Host: x\r\nz", opts);
            t.assert_invalid(out);
        }

        // A request line longer than `line_length` is rejected in request mode too.
        // ORACLE: decode_packet(http, <<"GET /x HTTP/1.1\r\nz">>, [{line_length,10}])
        //   => {error,invalid}
        let opt = t.option("line_length", 10);
        let opts = t.list(&[opt]);
        let out = t.decode("http", b"GET /x HTTP/1.1\r\nz", opts);
        t.assert_invalid(out);

        // ORACLE: a buffer with no line ending at all is rejected once it reaches
        // the ceiling: decode_packet(httph, <<"Host: x">>, [{packet_size,4}])
        //   => {error,invalid}
        let opt = t.option("packet_size", 4);
        let opts = t.list(&[opt]);
        let out = t.decode("httph", b"Host: x", opts);
        t.assert_invalid(out);

        // ORACLE: decode_packet(httph, <<"Host: x\r\n","moreandmore">>, [{packet_size,9}])
        //   => the header with the rest intact
        let opt = t.option("packet_size", 9);
        let opts = t.list(&[opt]);
        let out = t.decode("httph", b"Host: x\r\nmoreandmore", opts);
        let (_, rest) = t.ok(out);
        assert_eq!(rest, b"moreandmore");
    });
}

#[test]
fn a_valid_http_type_still_validates_its_arguments_first() {
    // The type/dispatch split must not weaken the shared argument validation.
    with_context(|t| {
        let not_a_binary = t.int(42);
        assert!(matches!(
            t.try_decode("http", b"GET / HTTP/1.1\r\n", not_a_binary),
            Err(BeamError::BadArg)
        ));

        // A `line_delimiter` option is `badarg` for any type but `line`.
        let bad = t.option("line_delimiter", 3);
        let opts = t.list(&[bad]);
        assert!(matches!(
            t.try_decode("http", b"GET / HTTP/1.1\r\n", opts),
            Err(BeamError::BadArg)
        ));
    });
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

#[test]
fn live_oracle_agrees_on_the_http_family() {
    // One live cross-check against the system ERTS: the shapes pinned above must
    // also come out of the real `erl` today. `~w` (not `~p`) is used so the
    // oracle does not line-wrap the output. Skips loudly when unavailable.
    let Some(output) = oracle_eval(
        r#"io:format("~w~n",[[
             erlang:decode_packet(http,<<"GET /x HTTP/1.1\r\n">>,[]),
             erlang:decode_packet(http_bin,<<"GET / HTTP/1.1\r\n">>,[]),
             erlang:decode_packet(http,<<"GET /\r\n">>,[]),
             erlang:decode_packet(http,<<"HTTP/1.1 200 OK\r\n">>,[]),
             erlang:decode_packet(http,<<"GET http://h:8/x HTTP/1.0\r\n">>,[]),
             erlang:decode_packet(httph,<<"Host: x\r\nz">>,[]),
             erlang:decode_packet(httph,<<"cONTENT-lENGTH: 5\r\nz">>,[]),
             erlang:decode_packet(httph_bin,<<"Host: x\r\nz">>,[]),
             erlang:decode_packet(httph,<<"\r\nz">>,[]),
             erlang:decode_packet(httph,<<"Host: x\r\n">>,[]),
             erlang:decode_packet(http,<<"X: y\r\n">>,[]),
             erlang:decode_packet(httph,<<"Host: x\r\nz">>,[{packet_size,8}])]])"#,
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "[{ok,{http_request,'GET',{abs_path,[47,120]},{1,1}},<<>>},\
         {ok,{http_request,'GET',{abs_path,<<47>>},{1,1}},<<>>},\
         {ok,{http_request,'GET',{abs_path,[47]},{0,9}},<<>>},\
         {ok,{http_response,{1,1},200,[79,75]},<<>>},\
         {ok,{http_request,'GET',{absoluteURI,http,[104],8,[47,120]},{1,0}},<<>>},\
         {ok,{http_header,14,'Host',[72,111,115,116],[120]},<<122>>},\
         {ok,{http_header,38,'Content-Length',[99,79,78,84,69,78,84,45,108,69,78,71,84,72],[53]},<<122>>},\
         {ok,{http_header,14,'Host',<<72,111,115,116>>,<<120>>},<<122>>},\
         {ok,http_eoh,<<122>>},\
         {more,undefined},\
         {ok,{http_error,[88,58,32,121,13,10]},<<>>},\
         {error,invalid}]",
        "the live oracle disagrees with the pinned goldens"
    );
}
