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

//! Erlang **source** rendering of the renderable generated subset.
//!
//! The oracle cross-checks hand `erl` Erlang source (not bytes), so the terms this
//! codec can render back to source are printed here and compared against
//! `term_to_binary(T)`. Kinds with no source-level construction (internal/external
//! identifiers, `NEW_FUN_EXT` funs, records) return `None` and are simply not part of
//! the oracle corpus.

use beam_codec::term::{
    big_parts, bitstring_bytes, cons_head, cons_tail, float_value, fun_function, fun_kind_term,
    fun_module, fun_true_arity_term, tuple_arity, tuple_element, FunKind,
};
use beam_term::prelude::{
    atom_val, is_atom, is_list, is_nil, is_small, signed_value, AtomTable, Eterm,
};

fn quote_atom(name: &str) -> String {
    let mut s = String::from("'");
    for c in name.chars() {
        match c {
            '\'' => s.push_str("\\'"),
            '\\' => s.push_str("\\\\"),
            _ => s.push(c),
        }
    }
    s.push('\'');
    s
}

fn render_float(f: f64) -> String {
    // `{:e}` is the shortest round-tripping decimal; Erlang needs a radix point.
    let s = format!("{f:e}");
    if s.contains('.') {
        s
    } else {
        match s.find('e') {
            Some(i) => format!("{}.0{}", &s[..i], &s[i..]),
            None => format!("{s}.0"),
        }
    }
}

fn render_big(negative: bool, digits: &[u64]) -> String {
    let mut parts = Vec::new();
    for (i, d) in digits.iter().enumerate() {
        if *d == 0 {
            continue;
        }
        if i == 0 {
            parts.push(format!("{d}"));
        } else {
            parts.push(format!("({d} bsl {})", 64 * i));
        }
    }
    let body = if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(" + ")
    };
    if negative {
        format!("-({body})")
    } else {
        format!("({body})")
    }
}

fn render_bitstring(bytes: &[u8], offset: usize, size: usize) -> Option<String> {
    let get = |start: usize, n: usize| -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            let bit = (bytes[(start + i) >> 3] >> (7 - ((start + i) & 7))) & 1;
            v = (v << 1) | u64::from(bit);
        }
        v
    };
    let full = size / 8;
    let rem = size % 8;
    let mut parts = Vec::with_capacity(full + 1);
    for i in 0..full {
        parts.push(format!("{}", get(offset + i * 8, 8)));
    }
    if rem > 0 {
        parts.push(format!("{}:{}", get(offset + full * 8, rem), rem));
    }
    Some(format!("<<{}>>", parts.join(",")))
}

fn render_list(t: Eterm, atoms: &AtomTable) -> Option<String> {
    let mut elems = Vec::new();
    let mut cur = t;
    loop {
        if is_nil(cur) {
            return Some(format!("[{}]", elems.join(",")));
        }
        if !is_list(cur) {
            return Some(format!("[{} | {}]", elems.join(","), render(cur, atoms)?));
        }
        elems.push(render(cons_head(cur).ok()?, atoms)?);
        cur = cons_tail(cur).ok()?;
    }
}

/// Erlang source for `t`, or `None` for a kind with no source-level construction here
/// (internal/external identifiers, `NEW_FUN_EXT` funs, records).
pub fn render(t: Eterm, atoms: &AtomTable) -> Option<String> {
    if is_nil(t) {
        return Some("[]".to_string());
    }
    if is_small(t) {
        return Some(signed_value(t).to_string());
    }
    if is_atom(t) {
        return Some(quote_atom(atoms.name(atom_val(t))?));
    }
    if is_list(t) {
        return render_list(t, atoms);
    }
    if let Ok(kind) = fun_kind_term(t) {
        if kind != FunKind::External {
            return None;
        }
        let m = quote_atom(atoms.name(fun_module(t).ok()?)?);
        let f = quote_atom(atoms.name(fun_function(t).ok()?)?);
        return Some(format!("fun {m}:{f}/{}", fun_true_arity_term(t).ok()?));
    }
    if let Ok((neg, digits)) = big_parts(t) {
        return Some(render_big(neg, digits));
    }
    if let Ok(v) = float_value(t) {
        return Some(render_float(v));
    }
    if let Ok(arity) = tuple_arity(t) {
        let mut parts = Vec::with_capacity(arity);
        for i in 0..arity {
            parts.push(render(tuple_element(t, i).ok()?, atoms)?);
        }
        return Some(format!("{{{}}}", parts.join(",")));
    }
    if let Ok(pairs) = beam_term::map::map_pairs(t, atoms) {
        let mut parts = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            parts.push(format!("{} => {}", render(k, atoms)?, render(v, atoms)?));
        }
        return Some(format!("#{{{}}}", parts.join(",")));
    }
    if let Ok((bytes, offset, size)) = bitstring_bytes(t) {
        return render_bitstring(bytes, offset, size);
    }
    None
}
