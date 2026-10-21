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

//! The `io_lib_pretty:cind/7` indentation probe used by the `~p`/`~P` re-flow.
//!
//! A safe submodule of `crate::print::pretty` (it registers no BIFs). `cind/7`
//! computes the indentation a tagged tuple's continuation lines want — or fails
//! with the oracle's `throw(no_good)` — and [`while_fail_indent`] is the
//! `while_fail([-1,4], …)` driver `super::pretty_chunk` runs to pick it.

use beam_common::Result;

use crate::error::badarg;

use super::{
    atomic, atomic_field, atomic_pair, last_depth, map_value_indent, Kind, Node, Seq, SeqTail, Tpl,
};

/// `io_lib_pretty:cind/7` — the indentation a tagged tuple's continuation lines
/// want, or `Err` on the oracle's `throw(no_good)`.
///
/// # Errors
///
/// `error:badarg` when no indentation lets the render fit within half the line
/// (the oracle's `no_good`).
fn cind(node: &Node, col: i64, ll: i64, m: i64, ind: i64, ld: i64, w: i64) -> Result<i64> {
    if node.len < ll - col - ld && node.len + w + ld <= m {
        return Ok(ind);
    }
    match &node.tpl {
        Tpl::Text(_) | Tpl::Bin(_) | Tpl::Pair(_, _) => Ok(ind),
        Tpl::Container(kind, seq) => match kind {
            Kind::List => cind_list(seq, col + 1, ll, m, ind, ld, w + 1),
            Kind::Tuple(true) => cind_tag_tuple(seq, col, ll, m, ind, ld, w + 1),
            Kind::Tuple(false) => cind_list(seq, col + 1, ll, m, ind, ld, w + 1),
            Kind::Map => cind_map(seq, col + 2, ll, m, ind, ld, w + 2),
        },
        // `cind({{record, [{_Name,NLen} | L]}, _Len, _, _}, Col, Ll, M, Ind, LD, W) ->
        //      cind_record(L, NLen, Col, Ll, M, Ind, LD, W + NLen + 1)`.
        Tpl::Record { parts, fields } => {
            cind_record(fields, parts.name_len, col, ll, m, ind, ld, w + parts.name_len + 1)
        }
        // A field is reached through [`cind_field`], never from `cind/7`; the
        // oracle's catch-all `cind({S,_Len,_,_}, _, _, _, Ind, _, _) -> Ind` answers.
        Tpl::Field { .. } => Ok(ind),
    }
}
/// `io_lib_pretty:cind_tag_tuple/7`.
fn cind_tag_tuple(seq: &Seq, col: i64, ll: i64, m: i64, ind: i64, ld: i64, w: i64) -> Result<i64> {
    let (tag, rest) = match seq.elems.split_first() {
        Some(split) => split,
        None => return Ok(ind),
    };
    let tag_ind = tag.len + 2;
    let tcol = col + tag_ind;
    if ind > 0 && tag_ind > ind {
        let col1 = col + ind;
        if m + col1 <= ll || col1 <= ll / 2 {
            cind_tail(rest, &seq.tail, col1, tcol, ll, m, ind, ld, w + tag.len)
        } else {
            Err(badarg())
        }
    } else if m + tcol < ll || tcol < ll / 2 {
        cind_elems(rest, &seq.tail, tcol, ll, m, ind, ld, w + tag.len + 1)
    } else {
        Err(badarg())
    }
}
/// `io_lib_pretty:cind_map/7`.
fn cind_map(seq: &Seq, col: i64, ll: i64, m: i64, ind: i64, ld: i64, w: i64) -> Result<i64> {
    match seq.elems.split_first() {
        Some((p, ps)) => {
            let pw = cind_pair(
                p,
                col,
                col,
                ll,
                m,
                ind,
                last_depth(!ps.is_empty(), ld),
                w,
            )?;
            cind_pairs_tail(ps, col, col + pw, ll, m, ind, ld, w + pw)
        }
        None => Ok(ind),
    }
}
/// `io_lib_pretty:cind_pairs_tail/7`.
fn cind_pairs_tail(
    ps: &[Node],
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    ind: i64,
    ld: i64,
    w: i64,
) -> Result<i64> {
    match ps.split_first() {
        None => Ok(ind),
        Some((p, rest)) => {
            let ld1 = last_depth(!rest.is_empty(), ld);
            let e_len = 1 + p.len;
            let flat = if ld1 == 0 {
                e_len + 1 < ll - col && w + e_len + 1 <= m && atomic_pair(p)
            } else {
                e_len < ll - col - ld1 && w + e_len + ld1 <= m && atomic_pair(p)
            };
            if flat {
                cind_pairs_tail(rest, col0, col + e_len, ll, m, ind, ld, w + e_len)
            } else {
                let pw = cind_pair(p, col0, col0, ll, m, ind, ld1, 0)?;
                cind_pairs_tail(rest, col0, col0 + pw, ll, m, ind, ld, pw)
            }
        }
    }
}
/// `io_lib_pretty:cind_pair/7`.
fn cind_pair(
    p: &Node,
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    ind: i64,
    ld: i64,
    w: i64,
) -> Result<i64> {
    if p.len < ll - col - ld && p.len + w + ld <= m {
        return Ok(if atomic_pair(p) { p.len } else { ll });
    }
    let (k, v) = match &p.tpl {
        Tpl::Pair(k, v) => (k.as_ref(), v.as_ref()),
        _ => return Ok(ll),
    };
    cind(k, col0, ll, m, ind, ld, w)?;
    let value_ind = map_value_indent(ind);
    cind(v, col0 + value_ind, ll, m, ind, ld, 0)?;
    Ok(ll)
}
/// `io_lib_pretty:cind_record/8` — the indentation a record's field sequence wants.
///
/// The oracle's `[]` and `{dots,…}` clauses are its catch-all
/// `cind_record(_, _Nlen, _Col, _Ll, _M, Ind, _LD, _W) -> Ind`, so an empty or
/// elided field sequence answers the caller's `Ind` instead of failing.
///
/// # Errors
///
/// As [`cind_rec`]/[`cind_field`] (the oracle's `no_good`).
#[allow(clippy::too_many_arguments)]
fn cind_record(
    fields: &Seq,
    name_len: i64,
    col0: i64,
    ll: i64,
    m: i64,
    ind: i64,
    ld: i64,
    w0: i64,
) -> Result<i64> {
    let (first, rest) = match fields.elems.split_first() {
        Some(split) => split,
        None => return Ok(ind),
    };
    let n_ind = name_len + 1;
    let (col, w) = cind_rec(n_ind, col0, ll, m, ind, w0)?;
    let fw = cind_field(
        first,
        col,
        ll,
        m,
        ind,
        last_depth(!rest.is_empty(), ld),
        w,
    )?;
    cind_fields_tail(rest, col, col + fw, ll, m, ind, ld, w + fw)
}
/// `io_lib_pretty:cind_fields_tail/8`.
///
/// `col0` is the record's continuation column (the one [`cind_rec`] produced) and
/// `col` the running one. The broken arm continues at `Col + FW`
/// (`cind_fields_tail(Fs, Col0, Col + FW, …)`) — the oracle's own asymmetry with its
/// map twin [`cind_pairs_tail`], which continues at `Col0 + PW`; reproduced as
/// written.
#[allow(clippy::too_many_arguments)]
fn cind_fields_tail(
    fields: &[Node],
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    ind: i64,
    ld: i64,
    w: i64,
) -> Result<i64> {
    match fields.split_first() {
        None => Ok(ind),
        Some((f, rest)) => {
            let ld1 = last_depth(!rest.is_empty(), ld);
            let e_len = 1 + f.len;
            let flat = if ld1 == 0 {
                e_len + 1 < ll - col && w + e_len + 1 <= m && atomic_field(f)
            } else {
                e_len < ll - col - ld1 && w + e_len + ld1 <= m && atomic_field(f)
            };
            if flat {
                cind_fields_tail(rest, col0, col + e_len, ll, m, ind, ld, w + e_len)
            } else {
                let fw = cind_field(f, col0, ll, m, ind, ld1, 0)?;
                cind_fields_tail(rest, col0, col + fw, ll, m, ind, ld, fw)
            }
        }
    }
}
/// `io_lib_pretty:cind_field/7` — a record field's width (`Len` for a leaf value,
/// `Ll` — force a newline — otherwise), or the oracle's `no_good`.
///
/// # Errors
///
/// As [`cind_rec`]/[`cind`].
fn cind_field(
    f: &Node,
    col: i64,
    ll: i64,
    m: i64,
    ind: i64,
    ld: i64,
    w: i64,
) -> Result<i64> {
    if f.len < ll - col - ld && f.len + w + ld <= m {
        return Ok(if atomic_field(f) { f.len } else { ll });
    }
    let (name_len, value) = match &f.tpl {
        Tpl::Field {
            name_len, value, ..
        } => (*name_len, value.as_ref()),
        _ => return Ok(ll),
    };
    let (col1, w1) = cind_rec(name_len, col, ll, m, ind, w + name_len)?;
    cind(value, col1, ll, m, ind, ld, w1)?;
    Ok(ll)
}
/// `io_lib_pretty:cind_rec/6` — a record field's continuation column and width
/// base, or the oracle's `throw(no_good)` when the column leaves no room (neither
/// `M + Col =< Ll` nor `Col =< Ll div 2`).
///
/// It is [`rec_indent`](super::rec_indent)'s rule without the separator chunk: the
/// same `Nl`/`DCol`, and `W` reset to `0` exactly when the line breaks.
///
/// # Errors
///
/// `error:badarg` (the oracle's `no_good`).
fn cind_rec(r_ind: i64, col0: i64, ll: i64, m: i64, ind: i64, w0: i64) -> Result<(i64, i64)> {
    let nl = ind > 0 && r_ind > ind;
    let d_col = if nl { ind } else { r_ind };
    let col = col0 + d_col;
    if m + col <= ll || col <= ll / 2 {
        Ok((col, if nl { 0 } else { w0 }))
    } else {
        Err(badarg())
    }
}
/// `io_lib_pretty:cind_list/7`.
fn cind_list(seq: &Seq, col0: i64, ll: i64, m: i64, ind: i64, ld: i64, w: i64) -> Result<i64> {
    cind_elems(&seq.elems, &seq.tail, col0, ll, m, ind, ld, w)
}
/// `io_lib_pretty:cind_list/7` over an already-split sequence.
fn cind_elems(
    elems: &[Node],
    tail: &SeqTail,
    col0: i64,
    ll: i64,
    m: i64,
    ind: i64,
    ld: i64,
    w: i64,
) -> Result<i64> {
    match elems.split_first() {
        None => Ok(ind),
        Some((e, rest)) => {
            let ld_first = last_depth(!rest.is_empty() || !matches!(tail, SeqTail::None), ld);
            let we = cind_element(e, col0, ll, m, ind, ld_first, w)?;
            cind_tail(rest, tail, col0, col0 + we, ll, m, ind, ld, w + we)
        }
    }
}
/// `io_lib_pretty:cind_tail/7`.
fn cind_tail(
    elems: &[Node],
    tail: &SeqTail,
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    ind: i64,
    ld: i64,
    w: i64,
) -> Result<i64> {
    match elems.split_first() {
        None => match tail {
            SeqTail::None | SeqTail::Dots(_) => Ok(ind),
            SeqTail::Improper(t) => {
                if t.len + 1 < ll - col - (ld + 1) && t.len + 1 + w + (ld + 1) <= m && atomic(t) {
                    Ok(ind)
                } else {
                    cind(t, col0, ll, m, ind, ld + 1, 0)
                }
            }
        },
        Some((e, rest)) => {
            let ld1 = last_depth(!rest.is_empty(), ld);
            let e_len = 1 + e.len;
            let flat = if ld1 == 0 {
                e_len + 1 < ll - col && w + e_len + 1 <= m && atomic(e)
            } else {
                e_len < ll - col - ld1 && w + e_len + ld1 <= m && atomic(e)
            };
            if flat {
                cind_tail(rest, tail, col0, col + e_len, ll, m, ind, ld, w + e_len)
            } else {
                let we = cind_element(e, col0, ll, m, ind, ld1, 0)?;
                cind_tail(rest, tail, col0, col0 + we, ll, m, ind, ld, we)
            }
        }
    }
}
/// `io_lib_pretty:cind_element/7`.
fn cind_element(e: &Node, col: i64, ll: i64, m: i64, ind: i64, ld: i64, w: i64) -> Result<i64> {
    if e.len < ll - col - ld && e.len + w + ld <= m && atomic(e) {
        Ok(e.len)
    } else {
        cind(e, col, ll, m, ind, ld, w)?;
        Ok(ll)
    }
}
/// `io_lib_pretty:while_fail([-1,4], fun(I) -> cind(…, I, …) end, 1)` — the
/// indentation the tagged-tuple path should use for this render: `-1` (no
/// special indent) or `4` when one of those makes `cind/7` succeed, else `1`.
pub(super) fn while_fail_indent(node: &Node, col: i64, ll: i64, m: i64) -> i64 {
    for candidate in [-1i64, 4] {
        if cind(node, col, ll, m, candidate, 0, 0).is_ok() {
            return candidate;
        }
    }
    1
}
