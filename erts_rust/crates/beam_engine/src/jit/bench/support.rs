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

//! Shared scaffolding for the engine's two `#[cfg(test)]` JIT measurement harnesses.
//!
//! The broad interpreter-vs-baseline tier table in [`super`] and the order-balanced
//! baseline-vs-Cranelift comparison in `crate::jit::cranelift::bench` were authored
//! independently and had grown their own copy of the pieces that are genuinely the
//! *same* in both. Those pieces live here, once:
//!
//! * [`arith_hook`] — the small-int `erlang` BIF hook every synthetic kernel imports
//!   (`+`/`-`/`*` plus the eight comparison operators; the comparison arms need the
//!   module's own `true`/`false` words, so they are optional). The native tiers
//!   compute the same words on their inlined fast paths, so this is the reference
//!   implementation those inlines must agree with, not the code under measurement.
//! * [`Fixture`] — the synthetic-[`Module`] builder: it wires the loader's own block
//!   decomposition ([`beam_loader::lower::build_blocks`]) into a
//!   `Module`/`Function`/`Export` with the imports the kernel needs.
//! * `Samples` — the per-tier sample vector with the `min`/`median` statistics.
//! * [`ns_per_op`]/[`ops_per_sec`] — the two report formatter primitives.
//!
//! What is deliberately **not** shared is each harness's *measurement strategy*: the
//! tier table in [`super`] measures every available tier in a fixed order, while
//! `crate::jit::cranelift::bench` measures the two native tiers in an ABBA order and
//! reports a paired ratio. Those stay in their own modules.

use std::time::Duration;

use beam_common::BeamError;
use beam_loader::{Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm};

use crate::context::BifHook;

// ---------------------------------------------------------------------------
// The one BIF hook both harnesses install
// ---------------------------------------------------------------------------

/// Small-int `erlang` BIF hook: `+`/`-`/`*` and the eight comparison operators.
///
/// Every synthetic kernel imports `erlang` BIFs, and the interpreter tier (and any
/// rejected operand on a native tier) reaches them through this hook. On a native tier
/// the inlined fast path computes the same small-int word itself, so this is the
/// **reference** implementation the inlines must agree with, not the code under
/// measurement.
///
/// The comparison operators return the module's **own** `true`/`false` words, so they
/// are only answered when `truthy`/`falsy` are supplied (the caller reads them out of
/// the module's atom table); without them a comparison is a `BadArg`, exactly as any
/// other unknown function is.
pub(crate) fn arith_hook(truthy: Option<Eterm>, falsy: Option<Eterm>) -> BifHook {
    Box::new(move |module, function, arity, args| {
        if module != "erlang" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        if !is_small(args[0]) || !is_small(args[1]) {
            return Err(BeamError::BadArith);
        }
        let (a, b) = (signed_value(args[0]), signed_value(args[1]));
        match function {
            "+" => Ok(make_small(a + b)),
            "-" => Ok(make_small(a - b)),
            "*" => Ok(make_small(a * b)),
            "=:=" | "==" if truthy.is_some() => Ok(if a == b { truthy } else { falsy }.unwrap()),
            "=/=" | "/=" if truthy.is_some() => Ok(if a != b { truthy } else { falsy }.unwrap()),
            "<" if truthy.is_some() => Ok(if a < b { truthy } else { falsy }.unwrap()),
            "=<" if truthy.is_some() => Ok(if a <= b { truthy } else { falsy }.unwrap()),
            ">" if truthy.is_some() => Ok(if a > b { truthy } else { falsy }.unwrap()),
            ">=" if truthy.is_some() => Ok(if a >= b { truthy } else { falsy }.unwrap()),
            _ => Err(BeamError::BadArg),
        }
    })
}

// ---------------------------------------------------------------------------
// Module construction
// ---------------------------------------------------------------------------

/// A synthetic module under construction: an owned atom table, the imports a kernel
/// chains on, and the one function the kernel exports.
pub(crate) struct Fixture {
    /// The module being built (its atom table interns every name used).
    module: Module,
}

impl Fixture {
    /// A module named `name`, importing nothing yet.
    pub(crate) fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    /// A module named `name` whose sole import is `erlang:'+'/2` (index 0).
    ///
    /// This is the shape every interpreter-vs-baseline workload uses; such a fixture
    /// can build its additions with [`Fixture::add`].
    pub(crate) fn with_plus(name: &str) -> Fixture {
        let mut fixture = Fixture::new(name);
        fixture.import("erlang", "+", 2);
        fixture
    }

    /// Intern `name` in the module's own atom table.
    ///
    /// The hand-built kernels need atoms beyond the ones a name or an import
    /// creates: the comparison kernel's `true`/`false` (a comparison returns the
    /// module's **own** atom word, never a fabricated boolean) and the `badmatch` a
    /// deopt path could name. Ungated — both the feature-on Cranelift kernels and the
    /// always-built interpreter-vs-baseline kernels need it.
    pub(crate) fn intern(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Add the import `module:function/arity` and return its reference.
    pub(crate) fn import(&mut self, module: &str, function: &str, arity: u32) -> ImportRef {
        let index = self.module.imports.len();
        let module = self.module.atom_table.intern(module);
        let function = self.module.atom_table.intern(function);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef(index as u32)
    }

    /// A `Dst = A + B` small-int addition (`gc_bif '+'/2`, no fail label).
    ///
    /// Uses the `erlang:'+'/2` import a [`Fixture::with_plus`] installs at index 0.
    pub(crate) fn add(&self, a: Operand, b: Operand, dst: Operand, live: u32) -> Instr {
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live,
            args: vec![a, b],
            dst,
        }
    }

    /// Assemble `code` as the sole function `name/arity`, export it, and return the
    /// module and the exported name atom.
    ///
    /// The blocks come from the loader's own decomposition
    /// ([`beam_loader::lower::build_blocks`]) — a native tier compiles **blocks**, so
    /// hand-rolled grouping would risk leaving a body without a native entry point.
    /// The entry label is the first label in `code` (which `build_blocks` preserves in
    /// order).
    pub(crate) fn finish(mut self, name: &str, arity: u32, code: Vec<Instr>) -> (Module, Atom) {
        let function = self.module.atom_table.intern(name);
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        let entry = blocks.first().map_or(Label(0), |block| block.label);
        self.module.exports.push(Export {
            function,
            arity,
            label: entry,
        });
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module: self.module.name,
            name: function,
            arity,
            code,
            blocks,
            labels,
        });
        (self.module, function)
    }
}

// ---------------------------------------------------------------------------
// Samples
// ---------------------------------------------------------------------------

/// The per-tier samples of one measurement.
///
/// Consumed by the order-balanced Cranelift harness (the only one that collects a
/// *distribution* of samples rather than a single aggregate), hence the gate: no
/// always-built harness here builds one, so without `jit-cranelift` **and** a unit-test
/// build the type would be dead code.
#[cfg(all(test, feature = "jit-cranelift"))]
pub(crate) struct Samples {
    /// The tier's name.
    pub(crate) tier: &'static str,
    /// One-time compile latency.
    pub(crate) compile: Duration,
    /// One ns/op sample per timed loop.
    pub(crate) ns_per_op: Vec<f64>,
}

#[cfg(all(test, feature = "jit-cranelift"))]
impl Samples {
    /// The minimum ns/op (the robust statistic under additive background load).
    pub(crate) fn min(&self) -> f64 {
        self.ns_per_op.iter().copied().fold(f64::INFINITY, f64::min)
    }

    /// The median ns/op.
    pub(crate) fn median(&self) -> f64 {
        let mut sorted = self.ns_per_op.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite ns/op"));
        let middle = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        } else {
            sorted[middle]
        }
    }
}

// ---------------------------------------------------------------------------
// Report formatter primitives
// ---------------------------------------------------------------------------

/// Nanoseconds per operation: `iterations` operations that took `elapsed` in total.
pub(crate) fn ns_per_op(elapsed: Duration, iterations: u32) -> f64 {
    elapsed.as_secs_f64() * 1e9 / f64::from(iterations)
}

/// Operations per second for `ns` nanoseconds per operation (`∞` when `ns` is zero).
pub(crate) fn ops_per_sec(ns: f64) -> f64 {
    if ns > 0.0 {
        1e9 / ns
    } else {
        f64::INFINITY
    }
}
