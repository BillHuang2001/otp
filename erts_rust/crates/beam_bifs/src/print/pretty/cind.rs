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

use super::{atomic, atomic_pair, last_depth, map_value_indent, Kind, Node, Seq, SeqTail, Tpl};

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
            SeqTail::None | SeqTail::Dots => Ok(ind),
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
