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

//! Compiling a **spawned** `-eval` closure into a real module.
//!
//! # Why this node exists
//!
//! The `-eval` evaluator is a recursive-descent tree walker ([`crate::eval`]'s `Evaluator`),
//! so a closure it creates has the **native Rust stack** for a continuation: nothing can park
//! it and resume it later. On the `-eval` route that is fine for a *synchronous*
//! `receive … after T` — the evaluator waits out its own window — but a closure
//! `erlang:spawn/1` started runs inside a scheduler slice, where re-entering the scheduler is
//! forbidden: a bare `receive` there is a typed gap and any call that parks
//! (`timer:sleep/1`) folds into the engine's `blocked()` and kills the process. The oracle's
//! child simply blocks until a message arrives.
//!
//! This node closes that gap by making such a closure **real code**: the closure's own source
//! text — sliced verbatim out of the `-eval` expression by the parser — is embedded in a
//! synthetic module whose parameters are the closure's captured bindings, compiled **in
//! process** by OTP's own compiler (contract A.9's route) and loaded with
//! `code:load_binary/3`. The spawned child is then an ordinary
//! [`ProcessEntry::Export`](crate::runner::ProcessEntry), i.e. a real engine machine and a real
//! process: a bare `receive` parks it and a later send resumes it, for free — exactly as
//! `spawn(timer, sleep, [200])` already parks today.
//!
//! # The generated module
//!
//! For a closure with captured bindings `V1 … Vk` (every key of the closure's environment, in
//! a deterministic name order — erl_eval's capture semantics: the body sees every binding in
//! scope) and arity `A`:
//!
//! ```text
//! -module(beam_rt_eval_fun_<index>).
//! -export([run/<k + A>]).
//! run(V1, …, Vk, A1, …, AA) -> (
//! <the closure's own source text, verbatim>
//! )(A1, …, AA).
//! ```
//!
//! A capture's parameter keeps the **captured variable's own name**, because the embedded
//! closure text reads that name.
//!
//! Embedding the *text* rather than printing the AST is deliberate: the Erlang compiler then
//! derives the closure's free variables, its optional self-recursion name
//! (`fun F(N) -> … F(N - 1) … end`) and every **nested** fun — none of which a hand-written
//! printer could be trusted to get right. A fun parameter shadows a captured binding of the
//! same name, exactly as erl_eval's clause binding does; a captured binding the body *rebinds*
//! becomes a match against that binding, again exactly as erl_eval behaves.
//!
//! # The compile itself, and why it is sound here
//!
//! The compile is driven **on the current context** — the one already dispatching
//! `erlang:spawn/1` — with `compile:file/2`'s `binary` option, so it runs the compiler in the
//! calling process (`no_spawn_compiler_process`) and answers the `.beam` image directly, with
//! no output file to collect. The source file itself is a uniquely named file under
//! [`std::env::temp_dir`], removed when [`SourceFile`] drops.
//!
//! Re-entering the dispatch machinery from inside a BIF is already the runtime's ordinary shape
//! — `erlang:apply/2,3` ([`crate::native`]'s `apply_dynamic`) and `os:cmd/1` both do it, and the
//! second one drives a whole external module through `run_export` just as `compile:file/2` does
//! here — and on the `-eval` route this context owns the process table and may pump the
//! scheduler, so the compiler's own file/io round trips run exactly as they do for a top-level
//! `compile:file/2`. The one place the compile is **not** attempted is
//! [`crate::native::process`]'s in-slice spawn arm, where re-entering the scheduler is
//! forbidden; a spawn made from inside a running engine keeps the tree-walking entry.
//!
//! # Failure is a decline, never a wrong value
//!
//! Every failure — no source span, a compile error (an unbound variable in the body, a syntax
//! the compiler rejects), an unloadable image, a compiler that cannot be reached — answers
//! `None`, and the spawn site then keeps the tree-walking entry
//! ([`ProcessEntry::Fun`](crate::runner::ProcessEntry)), whose typed gaps are the pre-existing
//! behaviour. The decision is **cached per closure index**, including a decline, so a closure
//! that cannot be compiled is not re-compiled on every spawn.

use beam_engine::{terms, ExecutionContext};
use beam_term::Eterm;

use crate::context::RuntimeContext;
use crate::runtime::Runtime;

use super::{FunKind, EVAL_FUN_UNIQ};

/// The name of the single function a compiled `-eval` closure exports.
pub(crate) const ENTRY_FUNCTION: &str = "run";

/// A compiled `-eval` closure: the synthetic module, its entry's arity, and the captured
/// values in that entry's parameter order.
///
/// Cached on [`Runtime::eval_compiled`], keyed by the closure's index in
/// [`Runtime::eval_closures`] — a closure's capture environment is fixed when the closure is
/// created, so the module and the capture order are fixed for that index too. The cache holds
/// copies of words the closure's own `FunBody` already names, so it opens **no new root
/// family**: the escaped-payload census reaches them through `FunBody::env_terms`.
#[derive(Clone)]
pub(crate) struct CompiledClosure {
    /// The synthetic module's name (`beam_rt_eval_fun_<index>`).
    pub(crate) module: String,
    /// The arity of its exported entry (`run/<captures + closure arity>`).
    pub(crate) arity: u32,
    /// The closure's captured values, in the generated module's parameter order.
    ///
    /// A spawn prepends these to the arguments it was given, so `x(0)` of the compiled
    /// `run/<k+A>` is the first captured binding and `x(k)` the first argument — the compiled
    /// closure's own machine then binds its parameters exactly as erl_eval's clause would.
    pub(crate) captures: Vec<Eterm>,
}

impl CompiledClosure {
    /// The argument list a spawn at `args` hands `module:run/arity`.
    ///
    /// The captures come first, in the generated module's parameter order, then the spawn's
    /// own arguments — so the arity always matches `run`'s, which is why
    /// [`entry_for`] refuses a closure spawned at another arity.
    pub(crate) fn arguments(&self, args: &[Eterm]) -> Vec<Eterm> {
        let mut combined = Vec::with_capacity(self.captures.len() + args.len());
        combined.extend_from_slice(&self.captures);
        combined.extend_from_slice(args);
        combined
    }

    /// Hands `f` the writable term slots of the captured values, one slice at a time.
    ///
    /// **This is a second copy of the owner closure's [`FunBody`](super::FunBody) capture
    /// environment** (a plain word copy, see [`compile`]), so it must be rewritten **in the
    /// same pass** as that environment: a walk that moved one without the other would leave
    /// this cache naming stale words.
    ///
    /// The contract shared by every per-owner `for_each_root_mut`: `f` may replace the
    /// **elements** of the slice, its **length** never changes, and the visitor must not depend
    /// on any ordering beyond the stored one. When `f` returns `Err` the slot writes it already
    /// made stay (they are made in place) and the error is returned; an empty capture list
    /// calls no visitor and answers `Ok(())`.
    pub(crate) fn for_each_root_mut(
        &mut self,
        f: &mut impl FnMut(&mut [Eterm]) -> beam_common::Result<()>,
    ) -> beam_common::Result<()> {
        if self.captures.is_empty() {
            return Ok(());
        }
        f(&mut self.captures)
    }
}

/// The entry point of the `-eval` closure fun `fun`, compiling and loading the synthetic module
/// on first use.
///
/// `None` means the closure keeps the tree-walking route: it is not one of this evaluator's
/// closures, its arity disagrees with the arguments the caller is spawning with (the oracle's
/// `badarity`), no source text was recorded for it, or the compile/load declined (see the
/// module docs). The answer is cached on the runtime — a hit costs one map probe and a clone of
/// the captured values.
pub(crate) fn entry_for(
    ctx: &mut RuntimeContext<'_>,
    fun: Eterm,
    args: &[Eterm],
) -> Option<CompiledClosure> {
    if terms::fun_uniq(fun).ok()? != EVAL_FUN_UNIQ {
        return None;
    }
    let index = terms::fun_index(fun).ok()?;
    let arity = terms::fun_arity(fun).ok()?;
    // A closure applied at an arity its clause list does not declare is the oracle's
    // `badarity`; that stays with the tree-walking entry (never a `run/k+A` of the wrong
    // shape), and the cache is deliberately **not** consulted or written here.
    if usize::try_from(arity).ok()? != args.len() {
        return None;
    }
    if let Some(cached) = ctx.runtime.eval_compiled.get(&index).cloned() {
        return cached;
    }
    let compiled = compile(ctx, index, arity);
    if compiled.is_none() {
        crate::debug_trace::trace(format_args!(
            "eval closure {index} cannot be compiled; its spawn keeps the tree-walking entry"
        ));
    }
    ctx.runtime.eval_compiled.insert(index, compiled.clone());
    compiled
}

/// Compiles, loads and answers the synthetic module for the closure stored at `index`.
fn compile(ctx: &mut RuntimeContext<'_>, index: u32, arity: u32) -> Option<CompiledClosure> {
    let spec = spec(ctx.runtime, index, arity)?;
    let module = format!("beam_rt_eval_fun_{index}");
    let text = module_source(&module, &spec);
    let started = std::time::Instant::now();
    let file = SourceFile::write(&module, &text)?;
    let image = compile_image(ctx, file.as_str())?;
    load(ctx, &module, &image)?;
    crate::debug_trace::trace(format_args!(
        "compiled eval closure {index} into {module} ({} captures + arity {arity}) in {} ms",
        spec.captures.len(),
        started.elapsed().as_millis()
    ));
    let arity = u32::try_from(spec.captures.len()).ok()? + arity;
    Some(CompiledClosure {
        module,
        arity,
        captures: spec.captures.into_iter().map(|(_, value)| value).collect(),
    })
}

/// One closure's compilation inputs: its own source text, its arity, and its captured bindings
/// (name-sorted, which is what makes the generated module's parameter order deterministic).
struct Spec {
    /// The closure's own source text (`fun … end`, verbatim).
    source: String,
    /// The closure's own arity — the number of parameters its clauses declare.
    closure_arity: usize,
    /// The closure's captured bindings, name-sorted.
    captures: Vec<(String, Eterm)>,
}

/// The compilation inputs of the closure stored at `index`, or `None` when it has no recorded
/// source text, is not a local closure, or disagrees with `arity`.
///
/// The environment's `_` key is dropped: it is the anonymous pattern, which erl_eval never
/// binds, and rendering it as a parameter name would collide with a wildcard.
fn spec(runtime: &Runtime, index: u32, arity: u32) -> Option<Spec> {
    let body = runtime.eval_closures.get(usize::try_from(index).ok()?)?;
    let FunKind::Local { clauses, .. } = &body.kind;
    let closure_arity = clauses.first().map_or(0, |clause| clause.params.len());
    // The fun term's own header arity and its clause list must agree, or the generated
    // `run/<k+A>` would apply the closure at an arity its text does not declare.
    if closure_arity != usize::try_from(arity).ok()? {
        return None;
    }
    let source = body.source.clone()?;
    let mut captures: Vec<(String, Eterm)> = body
        .env
        .iter()
        .filter(|(name, _)| name.as_str() != "_")
        .map(|(name, value)| (name.clone(), *value))
        .collect();
    captures.sort_by(|left, right| left.0.cmp(&right.0));
    Some(Spec {
        source,
        closure_arity,
        captures,
    })
}

/// The synthetic module's source text for `module` (see the module docs).
fn module_source(module: &str, spec: &Spec) -> String {
    let arguments = argument_names(spec.closure_arity, &spec.captures);
    let mut params: Vec<&str> = spec
        .captures
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    params.extend(arguments.iter().map(String::as_str));
    // The closure's own text sits on lines of its own, so a `%` comment on its last line can
    // never swallow the application's tail.
    format!(
        "-module({module}).\n\
         -export([{ENTRY_FUNCTION}/{}]).\n\
         {ENTRY_FUNCTION}({}) -> (\n{}\n)({}).\n",
        params.len(),
        params.join(", "),
        spec.source,
        arguments.join(", "),
    )
}

/// The names of the generated entry's own parameters, numbered from one.
///
/// The prefix grows until it collides with no captured binding's name: a capture named
/// `BeamRtArg1` would otherwise occupy the same parameter slot twice and the compiler would
/// reject the module.
fn argument_names(arity: usize, captures: &[(String, Eterm)]) -> Vec<String> {
    let mut prefix = String::from("BeamRtArg");
    loop {
        let names: Vec<String> = (1..=arity).map(|position| format!("{prefix}{position}")).collect();
        if !names
            .iter()
            .any(|name| captures.iter().any(|(capture, _)| capture == name))
        {
            return names;
        }
        prefix.push('_');
    }
}

/// A uniquely named `.erl` file under [`std::env::temp_dir`], removed when it goes out of scope.
struct SourceFile {
    /// The file's full path, for the cleanup.
    path: std::path::PathBuf,
    /// The same path as the string the compiler is handed.
    name: String,
}

impl SourceFile {
    /// Writes `text` as module `module`'s source.
    ///
    /// The name carries the module name, the process id and a per-call sequence number, so two
    /// compiles — of the same closure in two runtimes of one process, or of two closures at
    /// once — never share a path.
    fn write(module: &str, text: &str) -> Option<SourceFile> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "{module}_{pid}_{sequence}.erl",
            pid = std::process::id()
        ));
        let Some(name) = path.to_str().map(str::to_string) else {
            decline(
                "the source file",
                "the temporary directory is not a UTF-8 path",
            );
            return None;
        };
        match std::fs::write(&path, text) {
            Ok(()) => Some(SourceFile { path, name }),
            Err(err) => {
                decline("the source file", &format_args!("{}: {err}", path.display()));
                None
            }
        }
    }

    /// The path as the string form the compiler is handed.
    fn as_str(&self) -> &str {
        &self.name
    }
}

impl Drop for SourceFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            // A leftover temp file is not a runtime error; naming it keeps the cleanup honest.
            decline(
                "the source file's cleanup",
                &format_args!("{}: {err}", self.path.display()),
            );
        }
    }
}

/// `compile:file(Source, [binary, no_spawn_compiler_process, return_errors,
/// return_warnings])`, answering the `.beam` image.
///
/// `binary` is what keeps the artifact in memory (the compiler's answer is `{ok, Mod, Bin}`, or
/// `{ok, Mod, Bin, Warnings}` because warnings are returned); `no_spawn_compiler_process` keeps
/// the whole compile in this process; `return_*` keeps the compiler's diagnostics off stdout —
/// a warning the generated module earns (an unused captured binding, say) must never reach the
/// program's output.
fn compile_image(ctx: &mut RuntimeContext<'_>, source: &str) -> Option<Vec<u8>> {
    let source_term = match ctx.charlist(source) {
        Ok(term) => term,
        Err(err) => {
            decline("the source path", &err);
            return None;
        }
    };
    let mut options = Vec::with_capacity(4);
    for option in [
        "binary",
        "no_spawn_compiler_process",
        "return_errors",
        "return_warnings",
    ] {
        match ctx.atom(option) {
            Ok(atom) => options.push(atom),
            Err(err) => {
                decline("the option list", &err);
                return None;
            }
        }
    }
    let options = match terms::make_list(ctx, &options) {
        Ok(list) => list,
        Err(err) => {
            decline("the option list", &err);
            return None;
        }
    };
    let answer = match ctx.call_bif("compile", "file", 2, &[source_term, options]) {
        Ok(answer) => answer,
        Err(err) => {
            decline("compile:file/2", &err);
            return None;
        }
    };
    image_of(ctx, answer)
}

/// The `.beam` image out of `compile:file/2`'s answer, or `None` after tracing why the answer is
/// not one.
///
/// The tag is compared **by name** (`ok`), never by atom value: an atom value is a table-local
/// index, so the compiler's `ok` and this module's might be different words.
fn image_of(ctx: &mut RuntimeContext<'_>, answer: Eterm) -> Option<Vec<u8>> {
    let arity = terms::tuple_arity(answer).unwrap_or(0);
    let tag = terms::tuple_element(answer, 0)
        .ok()
        .and_then(|tag| ctx.atom_name(tag));
    let is_ok = tag.as_deref() == Some("ok");
    if !is_ok || !(arity == 3 || arity == 4) {
        decline_answer(ctx, "the compiler's answer", answer);
        return None;
    }
    let image = match terms::tuple_element(answer, 2).ok() {
        Some(image) => image,
        None => {
            decline_answer(ctx, "the compiled image", answer);
            return None;
        }
    };
    match terms::binary_bytes(image) {
        Ok(bytes) => Some(bytes.to_vec()),
        Err(err) => {
            decline("the compiled image", &err);
            None
        }
    }
}

/// `code:load_binary(Module, "", Image)` — the module goes into the code server's cache, from
/// which [`Runtime::resolve_export_entry`] then resolves the entry point.
///
/// The recorded filename is the empty list: the module was loaded **from memory**, and naming
/// the generated source (which is already gone) would make `code:which/1` point at a file that
/// never comes back.
fn load(ctx: &mut RuntimeContext<'_>, module: &str, image: &[u8]) -> Option<()> {
    let module_atom = match ctx.atom(module) {
        Ok(atom) => atom,
        Err(err) => {
            decline("the module atom", &err);
            return None;
        }
    };
    // The image is built on the runtime's pointer-stable `-eval` heap: it is handed to the code
    // server, which copies what it needs, and it outlives this call either way.
    let image_term = match beam_codec::term::make_binary(ctx.eval_heap(), image) {
        Ok(term) => term,
        Err(err) => {
            decline("the image binary", &err);
            return None;
        }
    };
    let file = match ctx.charlist("") {
        Ok(term) => term,
        Err(err) => {
            decline("the recorded filename", &err);
            return None;
        }
    };
    let answer = match ctx.call_bif("code", "load_binary", 3, &[module_atom, file, image_term]) {
        Ok(answer) => answer,
        Err(err) => {
            decline("code:load_binary/3", &err);
            return None;
        }
    };
    // `{module, Module}` is the only success shape; `{error, Reason}` (or anything else)
    // declines.
    let loaded = terms::tuple_arity(answer).is_ok_and(|arity| arity == 2)
        && terms::tuple_element(answer, 0)
            .ok()
            .and_then(|tag| ctx.atom_name(tag))
            .as_deref()
            == Some("module");
    if loaded {
        Some(())
    } else {
        decline_answer(ctx, "code:load_binary/3", answer);
        None
    }
}

/// Records a decline in the runtime's debug trace: what was attempted and why it stopped.
///
/// A decline is never a program error — the spawn site keeps the tree-walking entry — so this is
/// a diagnostic channel, silent unless `BEAM_RT_BOOT_TRACE` is set.
fn decline(what: &str, why: impl core::fmt::Display) {
    crate::debug_trace::trace(format_args!("eval closure compile: {what}: {why}"));
}

/// [`decline`] for a **term** answer from a lower layer (the compiler's diagnostic list, the
/// code server's reply), rendered through the registered printer.
///
/// Gated on the trace being on: rendering a term re-enters the dispatch machinery, which a
/// silent run must not pay for on a failure path.
fn decline_answer(ctx: &mut RuntimeContext<'_>, what: &str, answer: Eterm) {
    if !crate::debug_trace::enabled() {
        return;
    }
    ctx.runtime.reset_info_scratch();
    let runtime = &mut *ctx.runtime;
    let rendered = crate::format::term_to_string(
        &runtime.registry,
        &mut runtime.info_heap,
        runtime.code_server.atoms(),
        runtime.self_pid,
        answer,
    );
    match rendered {
        Ok(text) => decline(what, text.trim_end()),
        Err(err) => decline(what, &format_args!("<unprintable: {err}>")),
    }
}
