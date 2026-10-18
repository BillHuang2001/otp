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

//! The **`ets:new/2` option-validation matrix**, probed on the live oracle and
//! pinned from both sides.
//!
//! `tests/ets_options.rs` pins the *values* a creation option produces (the
//! `{write_concurrency, …}` / `{decentralized_counters, …}` cross-product). This
//! suite pins the complementary half: **which option forms are accepted at all**,
//! what is `error:badarg`, and what "last option wins" means for a repeated or
//! conflicting option — for every case, the oracle's verdict is compared with the
//! verdict the crate's own primitives produce when driven the way the `ets:new/2`
//! parser (`beam_bifs`) drives them.
//!
//! # The measured facts (system `erl`, OTP 29.1.1 / erts-17.1)
//!
//! Probing `ets:new(Name, Opts)` one option list at a time:
//!
//! | option | oracle verdict |
//! |---|---|
//! | bare `set` / `ordered_set` / `bag` / `duplicate_bag` | accepted (the kind) |
//! | bare `public` / `protected` / `private` | accepted (the protection) |
//! | bare `named_table`, `compressed` | accepted |
//! | `{keypos, N}` for `N >= 1` | accepted |
//! | `{keypos, 0}`, `{keypos, -1}`, `{keypos, foo}` | `badarg` |
//! | `{type, ordered_set}`, `{protection, public}`, `{compressed, true}` | `badarg` |
//! | a bare unknown atom (`foo`) | `badarg` |
//! | `{write_concurrency, true\|false\|auto}` | accepted |
//! | `{write_concurrency, {debug_hash_fixed_number_of_locks, N}}` for `1 =< N =< 32768` | accepted for `set`/`bag`/`duplicate_bag` |
//! | the same form on an `ordered_set` | `badarg` (a post-loop check) |
//! | `{write_concurrency, {debug_hash_fixed_number_of_locks, 0\|-1\|foo}}` | `badarg` |
//! | `{read_concurrency, true\|false}` | accepted |
//! | `{heir, none}`, `{heir, Pid}`, `{heir, Pid, Data}` | accepted |
//! | `{heir, none, Data}`, `{heir, foo}`, `{heir, Pid, D, X}` | `badarg` |
//! | `{decentralized_counters, true\|false}` | accepted |
//! | everything else (`{foo,1}`, `{foo,1,2}`, `notalist`, an improper list) | `badarg` |
//!
//! Repeated options are accepted and the **last one wins** (`{keypos,2},{keypos,3}`
//! yields `keypos 3`; `[bag,ordered_set]` is an `ordered_set`;
//! `[public,private]` is `private`; `[{heir,self()},{heir,none}]` installs no
//! heir), with two measured exceptions: the bare atoms `set` and `protected` are
//! **no-ops**, so `[ordered_set,set]` stays an `ordered_set` and
//! `[private,protected]` stays `private` (the C's `else if (val == am_set || val ==
//! am_protected) ;` arm, `erl_db.c:2710`).
//!
//! # What the crate owns and what it does not
//!
//! [`TableOptions`] is the **parsed** representation: its builders are typed
//! (`set_compressed(bool)`, `set_decentralized_counters(Option<bool>)`,
//! `set_heir(Option<Heir>)`, `set_keypos(usize)`), so a malformed *value*
//! (`{compressed, foo}`, `{heir, foo}`, `{keypos, -1}`) cannot even be expressed —
//! the `badarg` for it is the host parser's, and `tests/ets_access_options.rs`
//! records that split explicitly instead of pretending the crate validates a term
//! it never sees. The two range/combination checks the C does *after* its loop (the
//! `1..=32768` lock-count range and the `ordered_set` rejection of the explicit
//! granularity form) are likewise the parser's (`beam_bifs::ets::resolve`), and the
//! crate deliberately records the requested [`WriteConcurrency`] mode verbatim; the
//! last test states each of those handoffs with the exact primitive it needs.
//!
//! The oracle is **required by default** (a missing Erlang fails these checks;
//! `BEAM_ETS_ALLOW_NO_ORACLE=1` opts out, `BEAM_ETS_REQUIRE_ORACLE=1` forces it).

mod common;

use std::time::Duration;

use beam_ets::{Access, Heir, OwnerId, TableKind, TableOptions, WriteConcurrency};
use beam_term::prelude::*;
use common::oracle;

/// Wall-clock bound for the oracle child.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The `OwnerId` the crate-side heir cases use (a stand-in for the oracle's
/// `self()`, which this stage cannot express as a pid term).
fn heir_pid() -> OwnerId {
    OwnerId::new(4242)
}

// ---------------------------------------------------------------------------
// The shared shape both sides compare
// ---------------------------------------------------------------------------

/// The observable outcome of one `ets:new/2` option list, in the crate's own
/// primitive terms.
///
/// Everything is a `bool`/`Atom`/`usize` so the two sides can be compared without a
/// term decoder: `write_concurrency` is the **projection** of the mode
/// ([`TableOptions::write_concurrency`], which is `true` for `true`/`auto`/
/// `{debug_hash_fixed_number_of_locks, N}` alike), and the *raw* rendering
/// (`auto`, the lock tuple) is the host's (`TableInfo::write_concurrency` is a
/// `bool`) — pinned separately by
/// [`the_write_concurrency_mode_matches_the_oracles_raw_rendering`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    kind: TableKind,
    keypos: usize,
    access: Access,
    named_table: bool,
    compressed: bool,
    write_concurrency: bool,
    read_concurrency: bool,
    decentralized_counters: bool,
    heir_installed: bool,
}

impl Shape {
    /// The `ets:new/2` defaults (`set`, `keypos 1`, `protected`, nothing else).
    fn default_shape() -> Shape {
        Shape {
            kind: TableKind::Set,
            keypos: 1,
            access: Access::Protected,
            named_table: false,
            compressed: false,
            write_concurrency: false,
            read_concurrency: false,
            decentralized_counters: false,
            heir_installed: false,
        }
    }
}

/// Render the oracle's `~w`-printed shape (`{set,1,protected,...}`) as a [`Shape`].
fn parse_shape(text: &str) -> Option<Shape> {
    let inner = text.strip_prefix('{')?.strip_suffix('}')?;
    let fields: Vec<&str> = inner.split(',').collect();
    assert_eq!(
        fields.len(),
        9,
        "the oracle printed {} fields, expected 9: {text}",
        fields.len()
    );
    let boolean = |field: &str| match field {
        "true" => true,
        "false" => false,
        other => panic!("expected a boolean, found `{other}` in {text}"),
    };
    Some(Shape {
        kind: TableKind::from_atom_name(fields[0])
            .unwrap_or_else(|| panic!("unknown kind `{}` in {text}", fields[0])),
        keypos: fields[1]
            .parse()
            .unwrap_or_else(|_| panic!("keypos is not a number in {text}")),
        access: Access::from_atom_name(fields[2])
            .unwrap_or_else(|| panic!("unknown protection `{}` in {text}", fields[2])),
        named_table: boolean(fields[3]),
        compressed: boolean(fields[4]),
        write_concurrency: boolean(fields[5]),
        read_concurrency: boolean(fields[6]),
        decentralized_counters: boolean(fields[7]),
        heir_installed: boolean(fields[8]),
    })
}

// ---------------------------------------------------------------------------
// The crate-side model of the parser's builder calls
// ---------------------------------------------------------------------------

/// One builder call the `ets:new/2` parser makes for an accepted option — the
/// parser's translation of the option term into the [`TableOptions`] API.
///
/// A value the builder's type cannot carry (`{keypos, foo}`, `{compressed, foo}`, a
/// negative `keypos`, …) deliberately has **no** variant: those options never reach
/// the crate, and the case is recorded as [`Crate::Unrepresentable`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// A bare atom option: a kind name, a protection name, `named_table`,
    /// `compressed` — or an unknown atom, which every `from_atom_name` rejects.
    Atom(&'static str),
    /// `{keypos, N}` with `N >= 0`.
    Keypos(u64),
    /// `{write_concurrency, V}`: `"true"`, `"false"`, `"auto"` or `"locks<N>"`.
    WriteConcurrency(&'static str),
    /// `{read_concurrency, V}`: `"true"` or `"false"`.
    ReadConcurrency(&'static str),
    /// `{heir, …}`: `"none"`, `"pid"` (`{heir, Pid}`) or `"pid_data"`
    /// (`{heir, Pid, Data}`).
    Heir(&'static str),
    /// `{decentralized_counters, Bool}`.
    DecentralizedCounters(bool),
}

/// Drive the crate's builders exactly as the `ets:new/2` parser does, returning
/// `None` when the same input the oracle rejects is rejected here.
///
/// The `set`/`protected` **no-op** rule is the C's, so it is applied here (the
/// parser's job) — the crate's `set_kind`/`set_access` do overwrite, and it is the
/// parser that never calls them for those two atoms.
fn apply(calls: &[Call]) -> Option<TableOptions> {
    let mut options = TableOptions::new();
    for call in calls {
        match *call {
            Call::Atom(name) => match name {
                // Kinds: `set` never overrides an earlier kind (the C's no-op arm).
                "set" => {}
                "ordered_set" => {
                    options.set_kind(TableKind::OrderedSet);
                }
                "bag" => {
                    options.set_kind(TableKind::Bag);
                }
                "duplicate_bag" => {
                    options.set_kind(TableKind::DuplicateBag);
                }
                // Protections: `protected` likewise never overrides.
                "public" => {
                    options.set_access(Access::Public);
                }
                "protected" => {}
                "private" => {
                    options.set_access(Access::Private);
                }
                "named_table" => {
                    options.set_named(true);
                }
                "compressed" => {
                    options.set_compressed(true);
                }
                // An unknown atom is `badarg`: every name hook declines it.
                other => {
                    if TableKind::from_atom_name(other).is_some()
                        || Access::from_atom_name(other).is_some()
                    {
                        panic!("`{other}` is a known option name and has its own arm");
                    }
                    return None;
                }
            },
            Call::Keypos(keypos) => {
                let keypos = usize::try_from(keypos).ok()?;
                // `set_keypos` is the one fallible builder: it rejects `0`.
                options.set_keypos(keypos).ok()?;
            }
            Call::WriteConcurrency(value) => {
                let mode = match value.strip_prefix("locks") {
                    Some(count) => {
                        let count: u32 = count.parse().expect("a decimal lock count");
                        // The C accepts `1..=32768` (`DB_WRITE_CONCURRENCY_MIN_LOCKS`/
                        // `_MAX_LOCKS`); the crate records the requested count.
                        if count == 0 {
                            return None;
                        }
                        WriteConcurrency::FixedLocks(count)
                    }
                    None => WriteConcurrency::from_atom_name(value)?,
                };
                options.set_write_concurrency_mode(mode);
            }
            Call::ReadConcurrency(value) => {
                let read = match value {
                    "true" => true,
                    "false" => false,
                    other => panic!("`{other}` is not a boolean"),
                };
                options.set_read_concurrency(read);
            }
            Call::Heir(which) => {
                let data = match which {
                    "none" => {
                        options.set_heir(None);
                        continue;
                    }
                    "pid" => None,
                    "pid_data" => Some(Eterm::from_raw(0)),
                    other => panic!("`{other}` is not an heir form"),
                };
                options.set_heir(Some(Heir {
                    owner: heir_pid(),
                    data,
                }));
            }
            Call::DecentralizedCounters(value) => {
                options.set_decentralized_counters(Some(value));
            }
        }
    }
    Some(options)
}

/// The crate-side [`Shape`] of a built option set — read only through the public
/// getters, so it is exactly what `TableInfo`/`ets:info` would report.
fn shape_of(options: &TableOptions) -> Shape {
    Shape {
        kind: options.kind(),
        keypos: options.keypos(),
        access: options.access(),
        named_table: options.is_named(),
        compressed: options.is_compressed(),
        write_concurrency: options.write_concurrency(),
        read_concurrency: options.read_concurrency(),
        decentralized_counters: options.decentralized_counters(),
        heir_installed: options.heir().is_some(),
    }
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// How the crate side of a case is exercised.
#[derive(Debug, Clone, Copy)]
enum Crate {
    /// Fully expressible: the parser's builder calls, which the crate must accept.
    Calls(&'static [Call]),
    /// Not expressible: the builder's type cannot carry the malformed value (or the
    /// option form does not exist in the crate's model), so the `badarg` is the host
    /// parser's. The `reason` names the exact primitive.
    Unrepresentable(&'static str),
}

/// One creation-option case.
struct Case {
    /// The oracle-side tag (an Erlang atom, so snake_case).
    tag: &'static str,
    /// The option list, verbatim Erlang source.
    opts: &'static str,
    /// The oracle's answer: `None` = `error:badarg`.
    oracle: Option<Shape>,
    /// How the crate models the same request.
    krate: Crate,
}

/// The pinned matrix — every case probed on OTP 29.1.1 / erts-17.1.
fn cases() -> Vec<Case> {
    let default = Shape::default_shape();
    let set = |change: &dyn Fn(&mut Shape)| {
        let mut shape = default;
        change(&mut shape);
        shape
    };
    vec![
        // ---- the empty list and the bare kind/protection atoms -------------
        Case {
            tag: "empty",
            opts: "[]",
            oracle: Some(default),
            krate: Crate::Calls(&[]),
        },
        Case {
            tag: "set_bare",
            opts: "[set]",
            oracle: Some(default),
            krate: Crate::Calls(&[Call::Atom("set")]),
        },
        Case {
            tag: "ordered_set_bare",
            opts: "[ordered_set]",
            oracle: Some(set(&|s| s.kind = TableKind::OrderedSet)),
            krate: Crate::Calls(&[Call::Atom("ordered_set")]),
        },
        Case {
            tag: "bag_bare",
            opts: "[bag]",
            oracle: Some(set(&|s| s.kind = TableKind::Bag)),
            krate: Crate::Calls(&[Call::Atom("bag")]),
        },
        Case {
            tag: "duplicate_bag_bare",
            opts: "[duplicate_bag]",
            oracle: Some(set(&|s| s.kind = TableKind::DuplicateBag)),
            krate: Crate::Calls(&[Call::Atom("duplicate_bag")]),
        },
        Case {
            tag: "public_bare",
            opts: "[public]",
            oracle: Some(set(&|s| s.access = Access::Public)),
            krate: Crate::Calls(&[Call::Atom("public")]),
        },
        Case {
            tag: "protected_bare",
            opts: "[protected]",
            oracle: Some(default),
            krate: Crate::Calls(&[Call::Atom("protected")]),
        },
        Case {
            tag: "private_bare",
            opts: "[private]",
            oracle: Some(set(&|s| s.access = Access::Private)),
            krate: Crate::Calls(&[Call::Atom("private")]),
        },
        Case {
            tag: "named_table",
            opts: "[named_table]",
            oracle: Some(set(&|s| s.named_table = true)),
            krate: Crate::Calls(&[Call::Atom("named_table")]),
        },
        Case {
            tag: "named_twice",
            opts: "[named_table,named_table]",
            oracle: Some(set(&|s| s.named_table = true)),
            krate: Crate::Calls(&[Call::Atom("named_table"), Call::Atom("named_table")]),
        },
        Case {
            tag: "compressed",
            opts: "[compressed]",
            oracle: Some(set(&|s| s.compressed = true)),
            krate: Crate::Calls(&[Call::Atom("compressed")]),
        },
        // ---- keypos --------------------------------------------------------
        Case {
            tag: "keypos2",
            opts: "[{keypos,2}]",
            oracle: Some(set(&|s| s.keypos = 2)),
            krate: Crate::Calls(&[Call::Keypos(2)]),
        },
        Case {
            tag: "keypos_twice",
            opts: "[{keypos,2},{keypos,3}]",
            oracle: Some(set(&|s| s.keypos = 3)),
            krate: Crate::Calls(&[Call::Keypos(2), Call::Keypos(3)]),
        },
        Case {
            tag: "keypos0",
            opts: "[{keypos,0}]",
            oracle: None,
            krate: Crate::Calls(&[Call::Keypos(0)]),
        },
        Case {
            tag: "keypos_neg",
            opts: "[{keypos,-1}]",
            oracle: None,
            krate: Crate::Unrepresentable("TableOptions::set_keypos takes usize, so a negative keypos is unrepresentable"),
        },
        Case {
            tag: "keypos_foo",
            opts: "[{keypos,foo}]",
            oracle: None,
            krate: Crate::Unrepresentable("TableOptions::set_keypos takes usize, so a non-integer keypos is unrepresentable"),
        },
        // ---- the tuple forms the C never accepts ---------------------------
        Case {
            tag: "type_tuple",
            opts: "[{type,ordered_set}]",
            oracle: None,
            krate: Crate::Unrepresentable("there is no {type,_} option: TableOptions::set_kind is driven by the bare kind atom"),
        },
        Case {
            tag: "protection_tuple",
            opts: "[{protection,public}]",
            oracle: None,
            krate: Crate::Unrepresentable("there is no {protection,_} option at creation: TableOptions::set_access is driven by the bare protection atom"),
        },
        Case {
            tag: "compressed_tuple",
            opts: "[{compressed,true}]",
            oracle: None,
            krate: Crate::Unrepresentable("compressed is a bare flag atom, not a tuple: TableOptions::set_compressed(bool) is driven by the bare atom"),
        },
        Case {
            tag: "protection_foo",
            opts: "[{protection,foo}]",
            oracle: None,
            krate: Crate::Unrepresentable("the tuple form is rejected outright; Access::from_atom_name(\"foo\") additionally declines the name"),
        },
        // ---- last-wins vs the C's set/protected no-ops ---------------------
        Case {
            tag: "kind_last_wins",
            opts: "[bag,ordered_set]",
            oracle: Some(set(&|s| s.kind = TableKind::OrderedSet)),
            krate: Crate::Calls(&[Call::Atom("bag"), Call::Atom("ordered_set")]),
        },
        Case {
            tag: "set_after_ordered",
            opts: "[ordered_set,set]",
            oracle: Some(set(&|s| s.kind = TableKind::OrderedSet)),
            krate: Crate::Calls(&[Call::Atom("ordered_set"), Call::Atom("set")]),
        },
        Case {
            tag: "access_last_wins",
            opts: "[public,private]",
            oracle: Some(set(&|s| s.access = Access::Private)),
            krate: Crate::Calls(&[Call::Atom("public"), Call::Atom("private")]),
        },
        Case {
            tag: "protected_is_noop",
            opts: "[private,protected]",
            oracle: Some(set(&|s| s.access = Access::Private)),
            krate: Crate::Calls(&[Call::Atom("private"), Call::Atom("protected")]),
        },
        Case {
            tag: "public_then_protected",
            opts: "[public,protected]",
            oracle: Some(set(&|s| s.access = Access::Public)),
            krate: Crate::Calls(&[Call::Atom("public"), Call::Atom("protected")]),
        },
        // ---- write_concurrency ---------------------------------------------
        Case {
            tag: "wc_true",
            opts: "[{write_concurrency,true}]",
            oracle: Some(set(&|s| s.write_concurrency = true)),
            krate: Crate::Calls(&[Call::WriteConcurrency("true")]),
        },
        Case {
            tag: "wc_false",
            opts: "[{write_concurrency,false}]",
            oracle: Some(default),
            krate: Crate::Calls(&[Call::WriteConcurrency("false")]),
        },
        Case {
            tag: "wc_auto",
            opts: "[{write_concurrency,auto}]",
            oracle: Some(set(&|s| {
                s.write_concurrency = true;
                s.decentralized_counters = true;
            })),
            krate: Crate::Calls(&[Call::WriteConcurrency("auto")]),
        },
        Case {
            tag: "wc_locks64",
            opts: "[{write_concurrency,{debug_hash_fixed_number_of_locks,64}}]",
            oracle: Some(set(&|s| {
                s.write_concurrency = true;
                s.decentralized_counters = true;
            })),
            krate: Crate::Calls(&[Call::WriteConcurrency("locks64")]),
        },
        Case {
            tag: "wc_true_then_false",
            opts: "[{write_concurrency,true},{write_concurrency,false}]",
            oracle: Some(default),
            krate: Crate::Calls(&[
                Call::WriteConcurrency("true"),
                Call::WriteConcurrency("false"),
            ]),
        },
        Case {
            tag: "wc_foo",
            opts: "[{write_concurrency,foo}]",
            oracle: None,
            krate: Crate::Calls(&[Call::WriteConcurrency("foo")]),
        },
        Case {
            tag: "wc_locks0",
            opts: "[{write_concurrency,{debug_hash_fixed_number_of_locks,0}}]",
            oracle: None,
            krate: Crate::Calls(&[Call::WriteConcurrency("locks0")]),
        },
        Case {
            tag: "wc_locks_neg",
            opts: "[{write_concurrency,{debug_hash_fixed_number_of_locks,-1}}]",
            oracle: None,
            krate: Crate::Unrepresentable("WriteConcurrency::FixedLocks carries a u32, so a negative lock count is unrepresentable"),
        },
        Case {
            tag: "wc_locks_foo",
            opts: "[{write_concurrency,{debug_hash_fixed_number_of_locks,foo}}]",
            oracle: None,
            krate: Crate::Unrepresentable("WriteConcurrency::FixedLocks carries a u32, so a non-integer lock count is unrepresentable"),
        },
        Case {
            tag: "wc_locks_badtuple",
            opts: "[{write_concurrency,{foo,1}}]",
            oracle: None,
            krate: Crate::Unrepresentable("WriteConcurrency::from_atom_name covers the atom forms only; the hidden granularity tuple has its own tag"),
        },
        Case {
            tag: "locks_ordered",
            opts: "[ordered_set,{write_concurrency,{debug_hash_fixed_number_of_locks,8}}]",
            oracle: None,
            krate: Crate::Unrepresentable("the parser's post-loop check rejects the explicit granularity on an ordered_set; TableOptions records the requested mode"),
        },
        // ---- read_concurrency ------------------------------------------------
        Case {
            tag: "rc_true",
            opts: "[{read_concurrency,true}]",
            oracle: Some(set(&|s| s.read_concurrency = true)),
            krate: Crate::Calls(&[Call::ReadConcurrency("true")]),
        },
        Case {
            tag: "rc_twice",
            opts: "[{read_concurrency,true},{read_concurrency,false}]",
            oracle: Some(default),
            krate: Crate::Calls(&[
                Call::ReadConcurrency("true"),
                Call::ReadConcurrency("false"),
            ]),
        },
        Case {
            tag: "rc_foo",
            opts: "[{read_concurrency,foo}]",
            oracle: None,
            krate: Crate::Unrepresentable("TableOptions::set_read_concurrency takes bool, so a non-boolean flag is unrepresentable"),
        },
        // ---- decentralized_counters ------------------------------------------
        Case {
            tag: "dc_alone",
            opts: "[{decentralized_counters,true}]",
            oracle: Some(default),
            krate: Crate::Calls(&[Call::DecentralizedCounters(true)]),
        },
        Case {
            tag: "dc_with_wc",
            opts: "[{write_concurrency,true},{decentralized_counters,true}]",
            oracle: Some(set(&|s| {
                s.write_concurrency = true;
                s.decentralized_counters = true;
            })),
            krate: Crate::Calls(&[
                Call::WriteConcurrency("true"),
                Call::DecentralizedCounters(true),
            ]),
        },
        Case {
            tag: "dc_false_overrides",
            opts: "[{write_concurrency,auto},{decentralized_counters,false}]",
            oracle: Some(set(&|s| s.write_concurrency = true)),
            krate: Crate::Calls(&[
                Call::WriteConcurrency("auto"),
                Call::DecentralizedCounters(false),
            ]),
        },
        Case {
            tag: "dc_last_wins",
            opts: "[{write_concurrency,true},{decentralized_counters,true},{decentralized_counters,false}]",
            oracle: Some(set(&|s| s.write_concurrency = true)),
            krate: Crate::Calls(&[
                Call::WriteConcurrency("true"),
                Call::DecentralizedCounters(true),
                Call::DecentralizedCounters(false),
            ]),
        },
        Case {
            tag: "dc_foo",
            opts: "[{decentralized_counters,foo}]",
            oracle: None,
            krate: Crate::Unrepresentable("TableOptions::set_decentralized_counters takes Option<bool>, so a non-boolean value is unrepresentable"),
        },
        // ---- the private normalization (creation time only) -------------------
        Case {
            tag: "private_wc_true",
            opts: "[private,{write_concurrency,true}]",
            oracle: Some(set(&|s| s.access = Access::Private)),
            krate: Crate::Calls(&[Call::Atom("private"), Call::WriteConcurrency("true")]),
        },
        Case {
            tag: "private_rc_true",
            opts: "[private,{read_concurrency,true}]",
            oracle: Some(set(&|s| s.access = Access::Private)),
            krate: Crate::Calls(&[
                Call::Atom("private"),
                Call::ReadConcurrency("true"),
            ]),
        },
        Case {
            tag: "private_compressed",
            opts: "[private,compressed]",
            oracle: Some(set(&|s| {
                s.access = Access::Private;
                s.compressed = true;
            })),
            krate: Crate::Calls(&[Call::Atom("private"), Call::Atom("compressed")]),
        },
        Case {
            tag: "ordered_wc_true",
            opts: "[ordered_set,{write_concurrency,true}]",
            oracle: Some(set(&|s| {
                s.kind = TableKind::OrderedSet;
                s.write_concurrency = true;
                s.decentralized_counters = true;
            })),
            krate: Crate::Calls(&[
                Call::Atom("ordered_set"),
                Call::WriteConcurrency("true"),
            ]),
        },
        Case {
            tag: "ordered_dc_false",
            opts: "[ordered_set,{write_concurrency,true},{decentralized_counters,false}]",
            oracle: Some(set(&|s| {
                s.kind = TableKind::OrderedSet;
                s.write_concurrency = true;
            })),
            krate: Crate::Calls(&[
                Call::Atom("ordered_set"),
                Call::WriteConcurrency("true"),
                Call::DecentralizedCounters(false),
            ]),
        },
        // ---- the heir ---------------------------------------------------------
        Case {
            tag: "heir_none",
            opts: "[{heir,none}]",
            oracle: Some(default),
            krate: Crate::Calls(&[Call::Heir("none")]),
        },
        Case {
            tag: "heir_pid",
            opts: "[{heir,self()}]",
            oracle: Some(set(&|s| s.heir_installed = true)),
            krate: Crate::Calls(&[Call::Heir("pid")]),
        },
        Case {
            tag: "heir_pid_data",
            opts: "[{heir,self(),some_data}]",
            oracle: Some(set(&|s| s.heir_installed = true)),
            krate: Crate::Calls(&[Call::Heir("pid_data")]),
        },
        Case {
            tag: "heir_then_none",
            opts: "[{heir,self()},{heir,none}]",
            oracle: Some(default),
            krate: Crate::Calls(&[Call::Heir("pid"), Call::Heir("none")]),
        },
        Case {
            tag: "heir_none_then_pid",
            opts: "[{heir,none},{heir,self()}]",
            oracle: Some(set(&|s| s.heir_installed = true)),
            krate: Crate::Calls(&[Call::Heir("none"), Call::Heir("pid")]),
        },
        Case {
            tag: "heir_none_data",
            opts: "[{heir,none,some_data}]",
            oracle: None,
            krate: Crate::Unrepresentable("`none` with gift data has no Heir form: TableOptions::set_heir(None) is the arity-2 clear"),
        },
        Case {
            tag: "heir_foo",
            opts: "[{heir,foo}]",
            oracle: None,
            krate: Crate::Unrepresentable("TableOptions::set_heir takes Option<Heir>, whose owner is an OwnerId, so a non-pid heir is unrepresentable"),
        },
        Case {
            tag: "heir_4tuple",
            opts: "[{heir,self(),d,x}]",
            oracle: None,
            krate: Crate::Unrepresentable("Heir carries one Option<Eterm> of gift data, so a 4-tuple heir is unrepresentable"),
        },
        // ---- malformed option lists -------------------------------------------
        Case {
            tag: "not_a_list",
            opts: "notalist",
            oracle: None,
            krate: Crate::Unrepresentable("TableOptions is built from typed setters, so a non-list option argument never reaches it"),
        },
        Case {
            tag: "improper_list",
            opts: "[{keypos,2}|foo]",
            oracle: None,
            krate: Crate::Unrepresentable("TableOptions is built from typed setters, so an improper option list never reaches it"),
        },
        Case {
            tag: "unknown_atom",
            opts: "[foo]",
            oracle: None,
            krate: Crate::Unrepresentable("no name hook accepts `foo`: TableKind::from_atom_name and Access::from_atom_name both return None"),
        },
        Case {
            tag: "unknown_2tuple",
            opts: "[{foo,1}]",
            oracle: None,
            krate: Crate::Unrepresentable("there is no option named `foo`, so no builder call exists for it"),
        },
        Case {
            tag: "unknown_3tuple",
            opts: "[{foo,1,2}]",
            oracle: None,
            krate: Crate::Unrepresentable("there is no option named `foo`, so no builder call exists for it"),
        },
        Case {
            tag: "type_1tuple",
            opts: "[{type}]",
            oracle: None,
            krate: Crate::Unrepresentable("no builder takes a 1-tuple option"),
        },
    ]
}

// ---------------------------------------------------------------------------
// The oracle program
// ---------------------------------------------------------------------------

/// Build the escript source for `cases`: one `{Tag, Opts}` pair per case, and one
/// printed `Tag -> verdict` line per case.
fn oracle_program(cases: &[Case]) -> String {
    let mut source = String::from(
        "#!/usr/bin/env escript\n\
         %%! -noshell\n\
         main(_) ->\n\
         \x20   Cases = [\n",
    );
    for case in cases {
        source.push_str(&format!("        {{{}, {}}},\n", case.tag, case.opts));
    }
    source.push_str(
        "        ok\n\
         \x20   ],\n\
         \x20   [p(Tag, Opts) || {Tag, Opts} <- Cases],\n\
         \x20   ok.\n\
         \n\
         %% `write_concurrency` is projected to a boolean: the *raw* rendering is\n\
         %% `auto` / the lock tuple, which this suite pins separately.\n\
         bool(false) -> false;\n\
         bool(_) -> true.\n\
         \n\
         p(Tag, Opts) ->\n\
         \x20   R = try\n\
         \x20           T = ets:new(opttab, Opts),\n\
         \x20           S = {ets:info(T, type), ets:info(T, keypos), ets:info(T, protection),\n\
         \x20                ets:info(T, named_table), ets:info(T, compressed),\n\
         \x20                bool(ets:info(T, write_concurrency)),\n\
         \x20                bool(ets:info(T, read_concurrency)),\n\
         \x20                ets:info(T, decentralized_counters),\n\
         \x20                ets:info(T, heir) =/= none},\n\
         \x20           ets:delete(T),\n\
         \x20           S\n\
         \x20       catch error:badarg -> badarg\n\
         \x20       end,\n\
         \x20   io:format(\"~s -> ~w~n\", [Tag, R]).\n",
    );
    source
}

/// Run the oracle over `cases`, returning `tag -> verdict`.
fn run_oracle(cases: &[Case]) -> Vec<(String, Option<Shape>)> {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "the_oracle_creation_option_matrix_is_unchanged",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return Vec::new();
    };
    let program = oracle_program(cases);
    let output = oracle::run_escript(&escript, &program, &[], "options", ORACLE_TIMEOUT);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut verdicts = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (tag, verdict) = line
            .split_once(" -> ")
            .unwrap_or_else(|| panic!("unparsable oracle line: `{line}`"));
        let parsed = if verdict == "badarg" {
            None
        } else {
            Some(parse_shape(verdict).unwrap_or_else(|| {
                panic!("unparsable oracle shape for `{tag}`: `{verdict}`")
            }))
        };
        verdicts.push((tag.to_owned(), parsed));
    }
    assert_eq!(
        verdicts.len(),
        cases.len(),
        "the oracle answered {} of {} cases:\n{stdout}",
        verdicts.len(),
        cases.len()
    );
    verdicts
}

// ---------------------------------------------------------------------------
// 1. The oracle side
// ---------------------------------------------------------------------------

/// Every pinned case is still the oracle's verdict — the matrix cannot go stale
/// silently.
#[test]
fn the_oracle_creation_option_matrix_is_unchanged() {
    let cases = cases();
    let verdicts = run_oracle(&cases);
    if verdicts.is_empty() {
        return; // the oracle is genuinely absent and the opt-out was set
    }
    let mut failures = Vec::new();
    for (case, (tag, observed)) in cases.iter().zip(&verdicts) {
        assert_eq!(tag, case.tag, "the oracle's answer order shifted");
        if *observed != case.oracle {
            failures.push(format!(
                "{}: oracle {} but the matrix pins {}",
                case.tag,
                match observed {
                    None => "badarg".to_string(),
                    Some(shape) => format!("{shape:?}"),
                },
                match case.oracle {
                    None => "badarg".to_string(),
                    Some(shape) => format!("{shape:?}"),
                }
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "the pinned ets:new/2 matrix no longer matches the oracle:\n{}",
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 2. The crate side
// ---------------------------------------------------------------------------

/// Every case the crate can express reproduces the oracle's verdict and shape
/// through the public builder API — and every case it cannot express says exactly
/// which primitive cannot carry the value.
#[test]
fn the_crate_builder_reproduces_every_modelled_case() {
    let mut modelled = 0;
    for case in cases() {
        let Crate::Calls(calls) = case.krate else {
            continue;
        };
        modelled += 1;
        let built = apply(calls);
        match (built, case.oracle) {
            (Some(options), Some(expected)) => assert_eq!(
                shape_of(&options),
                expected,
                "{}: the builder's shape disagrees with the oracle ({})",
                case.tag,
                case.opts
            ),
            (None, None) => {}
            (Some(options), None) => panic!(
                "{}: the oracle rejects `{}` but the builder accepted it ({:?})",
                case.tag,
                case.opts,
                shape_of(&options)
            ),
            (None, Some(expected)) => panic!(
                "{}: the builder rejected `{}` but the oracle accepted it ({expected:?})",
                case.tag, case.opts
            ),
        }
    }
    assert!(
        modelled >= 40,
        "the matrix must exercise at least 40 builder-modelled cases, found {modelled}"
    );
}

/// The cases the crate deliberately cannot express: each records **why**, and the
/// named name hooks really do decline the malformed value.
#[test]
fn the_rejected_forms_the_typed_builder_cannot_express_are_the_parsers_badarg() {
    let mut unrepresentable = 0;
    for case in cases() {
        let Crate::Unrepresentable(reason) = case.krate else {
            continue;
        };
        unrepresentable += 1;
        assert!(
            !reason.is_empty(),
            "{}: an unrepresentable case must name the primitive that cannot carry the value",
            case.tag
        );
        assert!(
            case.oracle.is_none(),
            "{}: only a rejected form may be left to the parser (the oracle accepted it)",
            case.tag
        );
    }
    assert!(
        unrepresentable >= 15,
        "the unrepresentable half of the matrix shrank to {unrepresentable}"
    );

    // The name hooks the parser keys its `badarg` off really do decline an unknown
    // name, and the one fallible builder really does reject `keypos 0`.
    assert_eq!(TableKind::from_atom_name("foo"), None);
    assert_eq!(Access::from_atom_name("foo"), None);
    assert_eq!(WriteConcurrency::from_atom_name("foo"), None);
    assert!(
        TableOptions::new().set_keypos(0).is_err(),
        "set_keypos(0) is the crate's own badarg"
    );
    // …while every accepted name is recognised, so the parser's accept path has a
    // builder call for each.
    for name in ["set", "ordered_set", "bag", "duplicate_bag"] {
        assert!(TableKind::from_atom_name(name).is_some(), "{name}");
    }
    for name in ["public", "protected", "private"] {
        assert!(Access::from_atom_name(name).is_some(), "{name}");
    }
    for name in ["true", "false", "auto"] {
        assert!(WriteConcurrency::from_atom_name(name).is_some(), "{name}");
    }
}

/// The two checks the C performs *after* its option loop belong to the host parser,
/// and the crate's model is deliberately passive about them: it records what was
/// requested so the parser can reject the combination.
#[test]
fn the_post_loop_checks_are_the_parsers_and_the_model_stays_passive() {
    // (a) The `ordered_set` rejection of the explicit granularity form: the crate
    //     records the mode, it does not refuse it.
    let mut options = TableOptions::new();
    options.set_kind(TableKind::OrderedSet);
    options.set_write_concurrency_mode(WriteConcurrency::FixedLocks(8));
    assert_eq!(
        options.write_concurrency_mode(),
        WriteConcurrency::FixedLocks(8),
        "the requested lock count is recorded verbatim, so the parser can reject it"
    );
    assert!(options.write_concurrency());

    // (b) The `1..=32768` lock range: `FixedLocks` keeps even an out-of-range count,
    //     which is what lets the parser report `badarg` for it.
    assert_eq!(WriteConcurrency::FixedLocks(0).fixed_locks(), Some(0));
    assert_eq!(WriteConcurrency::FixedLocks(99_999).fixed_locks(), Some(99_999));

    // (c) …and a `private` table normalizes the mode off whatever it was, which is
    //     why `[private, {write_concurrency, {debug_hash_fixed_number_of_locks, N}}]`
    //     still reports no concurrency (the C's `is_fine_locked && !DB_PRIVATE`).
    let mut options = TableOptions::new();
    options.set_access(Access::Private);
    options.set_write_concurrency_mode(WriteConcurrency::FixedLocks(8));
    assert_eq!(options.write_concurrency_mode(), WriteConcurrency::Off);
    assert!(!options.write_concurrency());
    assert!(!options.decentralized_counters());
}

// ---------------------------------------------------------------------------
// 3. The raw `ets:info(T, write_concurrency)` rendering
// ---------------------------------------------------------------------------

/// The escript that prints the **raw** `ets:info(T, write_concurrency)` value for
/// the mode-bearing option lists — the form [`TableInfo`](beam_ets::TableInfo)'s
/// boolean cannot express.
const RAW_MODE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    Cases = [{wc_true,   [{write_concurrency,true}]},
             {wc_false,  [{write_concurrency,false}]},
             {wc_auto,   [{write_concurrency,auto}]},
             {wc_locks8, [{write_concurrency,{debug_hash_fixed_number_of_locks,8}}]},
             {wc_locks64,[{write_concurrency,{debug_hash_fixed_number_of_locks,64}}]},
             {wc_locks100,[{write_concurrency,{debug_hash_fixed_number_of_locks,100}}]},
             {wc_locks128,[{write_concurrency,{debug_hash_fixed_number_of_locks,128}}]},
             {wc_locks1024,[{write_concurrency,{debug_hash_fixed_number_of_locks,1024}}]},
             {private_wc_true, [private,{write_concurrency,true}]},
             {private_wc_auto, [private,{write_concurrency,auto}]},
             {auto_ordered, [ordered_set,{write_concurrency,auto}]}],
    [p(Tag, Opts) || {Tag, Opts} <- Cases],
    ok.

p(Tag, Opts) ->
    T = ets:new(rawmode, Opts),
    io:format("~s -> ~w~n", [Tag, ets:info(T, write_concurrency)]),
    ets:delete(T).
"#;

/// The mode the crate records is exactly the oracle's raw rendering — with the one
/// documented difference that the oracle reports the **normalized** lock count
/// (`N` rounded down to a power of two, clamped up to 64) while
/// [`WriteConcurrency::FixedLocks`] keeps the requested one.
#[test]
fn the_write_concurrency_mode_matches_the_oracles_raw_rendering() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "the_write_concurrency_mode_matches_the_oracles_raw_rendering",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        RAW_MODE_PROGRAM,
        &[],
        "raw_mode",
        ORACLE_TIMEOUT,
    );
    let observed: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();

    let expected = [
        "wc_true -> true",
        "wc_false -> false",
        "wc_auto -> auto",
        // The oracle normalizes the lock count: round down to a power of two, then
        // clamp up to NLOCKS_WITH_ITEM_COUNTERS (64).
        "wc_locks8 -> {debug_hash_fixed_number_of_locks,64}",
        "wc_locks64 -> {debug_hash_fixed_number_of_locks,64}",
        "wc_locks100 -> {debug_hash_fixed_number_of_locks,64}",
        "wc_locks128 -> {debug_hash_fixed_number_of_locks,128}",
        "wc_locks1024 -> {debug_hash_fixed_number_of_locks,1024}",
        // A private table never keeps the mode.
        "private_wc_true -> false",
        "private_wc_auto -> false",
        "auto_ordered -> auto",
    ];
    assert_eq!(
        observed, expected,
        "the oracle's raw write_concurrency rendering changed:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The crate's side: the atom forms render through `as_atom_name`, and the
    // explicit-granularity form keeps the *requested* count, which is what the host
    // normalizes when it renders the tuple.
    assert_eq!(WriteConcurrency::Off.as_atom_name(), Some("false"));
    assert_eq!(WriteConcurrency::On.as_atom_name(), Some("true"));
    assert_eq!(WriteConcurrency::Auto.as_atom_name(), Some("auto"));
    assert_eq!(WriteConcurrency::FixedLocks(64).as_atom_name(), None);
    assert_eq!(WriteConcurrency::FixedLocks(8).fixed_locks(), Some(8));

    // …and a `private` table reports no concurrency through the builder either.
    let mut options = TableOptions::new();
    options.set_access(Access::Private);
    options.set_write_concurrency_mode(WriteConcurrency::Auto);
    assert_eq!(options.write_concurrency_mode(), WriteConcurrency::Off);
    assert!(!options.write_concurrency());

    // The same table created `public` keeps the mode.
    let mut options = TableOptions::new();
    options.set_access(Access::Public);
    options.set_write_concurrency_mode(WriteConcurrency::Auto);
    assert_eq!(options.write_concurrency_mode(), WriteConcurrency::Auto);
}
