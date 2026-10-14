# `beam_rt::eval` — the `-eval` Erlang expression evaluator (`mod.rs` walker · `ast.rs` tree · `parse.rs` lexer/parser)

## Intent
The `-eval` evaluator (contract A.6): a lexer + Pratt parser (`parse.rs`) over an untyped AST (`ast.rs`) plus an `erl_eval`-style tree-walking evaluator (`mod.rs`) that `-eval Expr` runs.
It is driven by the CLI `-eval` action: `cli.rs::run`'s `Action::Eval(expr)` arm boots the runtime then calls `Runtime::eval` (`../runtime.rs`), which builds a `RuntimeContext` and calls `eval` here (`../runtime.rs::eval` → `crate::eval::eval`). `eval` is also re-exported at the crate root (`pub use crate::eval::eval;`) and in `prelude`.
Reused beyond the CLI: the evaluator's own closures can escape **into loaded modules** — `RuntimeContext::apply_fun` (`../context/mod.rs`, the `!External` fun arm) delegates a fun whose `uniq` is `EVAL_FUN_UNIQ` to `apply_eval_closure`, and the closure bodies live on `Runtime::eval_closures` (`Vec<Rc<FunBody>>`, so `lists:map(fun(X) -> X*X end, …)` applies one). `Runtime::eval` itself is used throughout the crate's tests as an expression/literal builder (e.g. `rt.eval("[call]")`).
The evaluator owns **no** term layout, arithmetic or printer: it dispatches down (`beam_term`/`beam_engine::terms`/`beam_bifs`) and inherits the lower crates' exact `erl` semantics.

## API Surface
### Entry points (`mod.rs`)
- `pub fn eval(ctx: &mut RuntimeContext<'_>, expr: &str) -> Result<Eterm>` — the crate-root-re-exported entry point: parses `expr`, tree-walks it in a fresh `Env`, and returns the value term on the process heap (valid for the runtime's lifetime). The whole driver runs inside `ctx.with_relocation_exempt(..)`, so an evaluation's Rust-local terms (the `Env`, reply terms) are never moved by a delivery-driven collection while the receive pump runs.
- `pub(crate) fn apply_eval_closure(ctx, fun, index, args) -> Result<Eterm>` — applies a stored evaluator closure by `index`, called from `RuntimeContext::apply_fun` when a loaded module applies a fun stamped `EVAL_FUN_UNIQ`. `BadArg` when no closure is stored at `index`.
- `pub(crate) const EVAL_FUN_UNIQ: u64 = 0x6265_616d_7274_0001` — the sentinel `uniq` stamped into every fun this evaluator builds, so such a fun is recognisable and resolvable.
- `pub(crate) struct FunBody { kind: FunKind, env: Env }` — a closure body (`FunKind::Local { name, clauses }` + its captured `env`), pushed onto `Runtime::eval_closures` by `register_fun`.
- `parse::parse(src) -> Result<Expr>` (`parse.rs`) and the AST (`ast.rs`) are reachable only inside this module (`mod ast; mod parse;` are private).
- Internal to `mod.rs`: `Evaluator` (the walker), `Flow { Raise{class,reason,origin}, Fatal }` + `EResult`/`Env`, `finish` (control-flow → `Result`), the `BitBuf` MSB-first binary builder, and `operator_bif`/`read_bits_be`/`read_bits_le`/`sign_extend`/`mask_bits`/`trim_digits`/`integer_segment_is_little` helpers.

### Grammar coverage (`parse.rs` + `ast.rs`)
The **expression** subset of Erlang (`erl_eval`'s input) with Erlang's exact precedence/associativity (`binop_of`'s binding powers mirror `erl_parse.yrl`; `catch` lowest, then `=`/`!`, `orelse`, `andalso`, the comparison family, `++`/`--`, additive, multiplicative, then prefix operators):
- literals in every base (`Base#Digits`, radix 2..=36), floats (with exponent), atoms (bare and quoted), strings, chars (`$a`, escapes incl. `\xHH`, `\x{…}`, octal, the named escapes), lists (incl. improper `[1,2|3]`), tuples, maps, binaries;
- variables (`X`, `_`, `_Name`) and `Pattern = Expr` matching;
- `case`/`if`/`begin`/`try`/`catch`/`receive`;
- `fun`s (anonymous `fun(…)->…end`, named self-recursive `fun F(…)->…end`, captured `fun f/A` / `fun M:F/A`);
- list, map and binary comprehensions (with `<-` generators, `<=` binary generators, and filters), incl. the `<<…>>` template form the parser records as a single segment;
- prefix/postfix call application: an auto-imported local call `f(Args)`, a remote call `m:f(Args)`, and a call of a fun-valued expression `(fun…end)(Args)`.
A trailing `.` is tolerated by the parser, and the CLI strips one (`cli.rs::strip_trailing_dot`).
Integers are exact up to `i128`; a literal past `i128` is a typed capability gap.
The binary-segment parse modes (`in_segment` for `/` as a type specifier rather than division, `no_remote` for a `try` clause's `Class:Reason` head) are the parser's two context flags.
Semantics inherited from the lower crates (nothing re-implemented): arithmetic/comparison/bit operators are **`erlang` BIFs** dispatched through `RuntimeContext::call_bif` (so `1/0` ⇒ `error:badarith` and the six bit operators get their bignum semantics); a call is a `call_bif` (the load-on-demand stdlib path); terms are built through `beam_engine::terms`/`beam_codec::term`; terms are inspected only through `beam_term`/`beam_engine::terms`. The operators the `beam_bifs` surface deliberately does **not** own — `and`/`or`/`xor`/`not`/`andalso`/`orelse` (Erlang special forms) and `++`/`--` — are implemented here with the oracle's error behaviour (`not 5` ⇒ `error:badarg`). The raise helpers (`error/1,2`, `exit/1`, `throw/1`, `nif_error/1,2`) and `apply/2,3` are served here so a **live reason term** survives (the atom-only `beam_common::ExitReason` cannot carry one). `erlang:raise/3` is deliberately **not** served here: it must validate its class *and* its stacktrace argument and forward the decoded trace, so the call falls through to the registered `beam_bifs::convert::raise_3` BIF (an invalid shape is that BIF's **returned** `badarg` atom — a value, never a raise — and a valid one raises with the decoded `StackTrace` attached).

### Capability gap (anything outside the grammar)
Everything outside the grammar is a typed capability gap: the parser (and a few evaluator arms) raise a `BeamError` built by `crate::error::bad_eval`/`error::unsupported`, whose reason atom starts with `unsupported` (the `unsupported:` convention). `Evaluator::flow_from_error` turns it into `Flow::Fatal`, so a raise from the evaluated expression's own `try … catch` cannot swallow it — it propagates out and the CLI exits **3** with one `beam_rt: unsupported: <reason>` line (`crate::error::unsupported_reason` is the single classifier). `erlang:halt/0,1` is likewise `Flow::Fatal` (uncatchable), so `… halt(0) catch … end` terminates with status 0. Genuine `error`/`exit`/`throw` raises (and BIF structured reasons like `{badmap,M}`, `{badkey,K}`, `{case_clause,V}`, `{bad_generator,X}`) stay fully catchable, with their live reason term preserved.

## Constraints
- **`unsafe`-free**: the crate is `#![deny(unsafe_code)]`; this node contains no `unsafe` (term layout is never touched here).
- **Return-based errors, no silent swallowing** (`../CONTEXT.md` §3): the public entry points return `beam_common::Result`; the internal walker carries its own `Flow` signal, converted once by `finish`. No crate-local error enum, no `panic!`/`unwrap()` on runtime data.
- **Call down, never duplicate**: arithmetic/comparison/bit operators and every call go through `RuntimeContext::call_bif` (the `beam_bifs` `erlang` registry / load-on-demand stdlib); terms are built/inspected only through `beam_term` (+ `beam_engine::terms` / `beam_codec::term`); the `~w`/`~p` printer is not re-implemented here.
- **`Flow::Fatal` is the uncatchable channel** (halt + capability gap); only `BeamError::Raise` becomes a catchable `Flow::Raise`.
- Files ~1000 lines: `mod.rs` is one cohesive unit (the `Evaluator`, the entry points and the `#[cfg(test)]` suite); `parse.rs`/`ast.rs` are separate by responsibility.

## Known Issues (typed capability gaps — never wrong results)
- `receive` **without** an `after` clause is a typed `unsupported` when nothing runnable can deliver: `-eval` has no host timer to block on. With `after T` the evaluator scans the running process's **real mailbox**, pumps the scheduler (bounded by `RECEIVE_PUMP_SLICES = 64`), and waits on the port poller for the remaining time, so `spawn(fun() -> Self ! hello end), receive M -> M after 1000 -> …` works end to end. A bare `receive` **inside a spawned process** is also `unsupported` (it runs inside a scheduler slice and may not re-enter the scheduler), and `receive … after infinity` maps to `infinity`'s "block forever" as a typed gap.
- `catch Expr` yields `{'EXIT', Reason}` for the `error`/`exit` classes; M1 evaluates without a stacktrace (the oracle appends one). A `try … catch Class:Reason:Stacktrace` clause's third pattern is matched against `[]` unconditionally, so the variable binds the empty list whatever the raise carried — the decoded `StackTrace` `erlang:raise/3` attaches is **not** surfaced by the evaluator's own catch clauses.
- Binary **segment** kinds `float`, `utf16` and `utf32` are typed `unsupported`; an integer pattern segment wider than 64 bits and a non-final unsized `binary`/`bitstring` pattern segment are too. An integer pattern value beyond the small-int range is `system_limit` (no bignum is built there).
- A non-empty list's clause `apply_closure` with no matching arity is `BadArity`.
- `erlang:alias/0`, NIF/driver dispatch, `open_port` and every other host facility behave as the lower crates specify — this node only *calls* them.

## Test Strategy
- In-module `#[cfg(test)] mod tests` in `mod.rs`: arithmetic + precedence, bit operators through the registry (incl. bignum round-trips), integer literals past the small range materializing a bignum and past `i128` being a typed gap, improper-list literals, arbitrary-precision integer segments, comparison/boolean operators, `case`/`if`/pattern matching, guards + clause fall-through, exceptions caught by class/reason, funs created/applied/captured, `fun M:F/A` as a real `EXPORT_EXT` term (the `wire/export_ext_encode` golden), comprehensions, `system_info` through the native floor, `halt` uncatchable, a capability gap uncatchable, a loaded module applying an `-eval` closure, `nif_error` carrying its live reason term, malformed expressions as gaps, and the evaluation-order gate `literals_evaluate_their_sub_expressions_in_source_order`. Two helpers enforce contract hygiene: `run` asserts a result is an **immediate** (a heap-allocated term may not outlive its runtime); `bits`/`ints`/`printed` copy values out.
- In-module `#[cfg(test)] mod tests` in `parse.rs`: literals (all bases, floats, atoms, strings, chars, `[]`), compound literals, operator precedence/associativity, `/` inside a binary segment being a specifier, control forms, comprehensions, the `<=` binary generator vs `=<`, a segment size not swallowing its type specifier, calls, the multi-element improper-list tail, an integer literal past `i64` staying exact, malformed input as a typed error, and a tolerated trailing dot.
- Integration (`../../tests/`): `eval.rs` — the differential `Eval` corpus's 17 goldens pinned at the CLI boundary (`env!("CARGO_BIN_EXE_beam_rt")`, the corpus's `try … catch` probe wrapper + argv), plus structured raise reasons + map key order, the two `semantics/*` binary probes, the five `boot/*` spawn/receive probes, the bare-receive `unsupported` boundary, the A.4 port probes, and the `wire/new_fun_ext_encode` `unsupported: local_fun` gap; and `eval_order.rs` — the **oracle-cross-checked** evaluation-order gate (see Notes for Agents), added in commit `97a3ea35ea`.
- The compatibility gate (`../../tools/differential/`) is the end-to-end check; this node's corpus probes are the `Eval`/`semantics`/`boot` groups.

## Routing Table
- `mod.rs` → the tree-walking `Evaluator` (expressions, patterns, guards, comprehensions, the `BitBuf` binary builder), `Flow`/`EResult`/`Env`, the entry points (`eval`, `apply_eval_closure`, `EVAL_FUN_UNIQ`, `FunBody`), `finish`/`flow_from_error`, and the colocated unit suite (incl. the evaluation-order gate).
- `ast.rs` → the untyped AST: `Expr` (used for both expressions and patterns) plus `BinOp`/`UnOp`/`Seg`/`SegKind`/`Endian`/`Clause`/`CatchClause`/`FunClause`/`Try`/`Receive`/`Qual`/`MapField` and `is_var_name`.
- `parse.rs` → the `Lexer` (tokens, all bases, floats, strings/chars + escapes, comments) and the Pratt `Parser` (`parse`, the `in_segment`/`no_remote` context flags, all control forms), plus the colocated unit suite.

## Notes for Agents
### The evaluation-order contract (sub-expressions run in `erl_eval`'s order)
The evaluator evaluates a construct's sub-expressions **left-to-right in source order**, exactly as `erl_eval` does:
- a **list**'s elements left-to-right, **then** its optional tail;
- a **tuple**'s elements left-to-right;
- a **map field**'s key before its value, with fields in source order;
- a **binary segment**'s value before its size;
- **call** arguments in order; `++`/`--` left operand before right;
- a **comprehension**'s template once per generated element in generator order.
Order is observable only through a side effect (an `io:format/2` in an element), so it is pinned by the spy-closure probes in `mod.rs::tests::literals_evaluate_their_sub_expressions_in_source_order` and in `../../tests/eval_order.rs` (the latter cross-checks the **system `erl`** prints the same bytes).
The list's **cons chain** is only *built* right-to-left after the values are already evaluated (`Evaluator::list` collects the element values, then folds them onto the tail) — this does not change the list's *value*, only the order the side effects ran in. Do not describe the current order as "right-to-left".
### Other facts
- The evaluator keeps its environment in Rust locals, so its terms are **not** in a collection's root set; the whole `eval` driver therefore runs under `RuntimeContext::with_relocation_exempt` (see `../context/CONTEXT.md`), which is exactly when a reply to this pid could arrive.
- A fun built here is recorded in `Runtime::eval_closures` and can escape into a loaded module; `fun M:F/A` is **not** a closure — it is the real `EXPORT_EXT` term (no `uniq`), applied through the host's ordinary dynamic call.
- `../CONTEXT.md` (this crate) describes the CLI surface and exit codes; `../runtime/CONTEXT.md` owns `Runtime::eval`/`eval_closures`; `../context/CONTEXT.md` documents `apply_fun`'s dispatch to `apply_eval_closure`.

## See Also
- `../cli/CONTEXT.md` → the `-eval` action and the four exit codes.
- `../context/CONTEXT.md` → `RuntimeContext` (the host), `apply_fun`, and the relocation-exemption seam.
- `../runtime/CONTEXT.md` → `Runtime::eval`, `Runtime::eval_closures`, `pump_scheduler` (the receive pump).
- `../CONTEXT.md` → the crate-level CLI subset and the `ERTS_RUST_REWRITE_*` design docs (A.6).
- `../../tools/differential/` → the compatibility gate whose `Eval`/`semantics`/`boot` corpus this node serves.
