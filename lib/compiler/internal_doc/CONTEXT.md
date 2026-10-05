# Compiler Internal Documentation

## Intent
In-repo developer documentation of `lib/compiler` internals, aimed at compiler contributors rather than end users.
Not the user reference manual (that lives in `../doc/`), and not a doc-build input by default — these are hand-written markdown notes describing invariants, checks and gotchas of the compiler's intermediate representations.
Does not contain any code, tests or build files.

## API Surface
- `beam_ssa.md` — invariants on the structure and format of BEAM SSA (exception handling / `try`-`catch` vs old-style `catch` translation, landing pads, exception re-issuing via `resume`, `succeeded:body` after non-tail calls, variable naming and the `cnt` counter).
- `ssa_checks.md` — user guide for the compiler's built-in SSA checker: `%ssa%` comment syntax, `<expected-result>` (`pass`/`fail`/`xfail`), formals, the `<pipeline-location>` (`post_ssa_opt`), matching semantics and compiler integration via `{check_ssa,post_ssa_opt}`.
- `cerl-notes.md` — notes on Core Erlang (`cerl` modules), in particular why maps are not optimized in `cerl_clauses:match/3` and how the `cerl_inline` two-pass assumption breaks for variable-dependent data types.
- These are plain markdown texts, not ExDoc module docs; they are embedded into the generated docs only where listed as ExDoc `extras` (see `../doc/docs.exs`).

## Constraints
- Every file begins with the standard OTP SPDX copyright header (`%CopyrightBegin%` … `%CopyrightEnd%`, `SPDX-License-Identifier: Apache-2.0`); keep it intact when editing.
- `beam_ssa.md` and `ssa_checks.md` are referenced as ExDoc `extras` from `../doc/docs.exs` — their paths and headings are consumed by the doc build, so renaming/moving them requires updating that reference.
- `cerl-notes.md` is not referenced from any build or doc configuration; it exists purely as an in-repo note.
- Content documents current compiler behaviour; when the compiler's SSA/Core invariants change the corresponding text must be updated (these are hand-maintained, nothing regenerates them).

## Notes for Agents
- Audience is compiler developers: assume familiarity with SSA/Core Erlang terminology; no end-user-level explanation is expected.
- `ssa_checks.md` documents an internal *testing* feature (the SSA checker) — the authoritative grammar is the `ssa_check_when_clause` production in `../src/erl_parse.yrl`.
- `beam_ssa.md` and `ssa_checks.md` describe BEAM SSA, whose implementation lives in `../src/beam_ssa*` and the SSA optimization passes in `../src/ssa_opt*`/`../src/beam_ssa_opt.erl`.
- `cerl-notes.md` describes issues around `cerl_clauses`/`cerl_inline`, implemented under `../src/cerl*` (mostly historical background on Core Erlang, predating the current BEAM-only pipeline).

## Routing Table
| File | Description |
| --- | --- |
| `beam_ssa.md` | Structural/format invariants of BEAM SSA (try/catch & exception handling, `succeeded:body`, variable naming) |
| `ssa_checks.md` | Documentation of the SSA checker (`%ssa%` assertions) used to test SSA transforms |
| `cerl-notes.md` | Notes on Core Erlang modules, esp. `cerl_clauses:match/3` and `cerl_inline` |

## See Also
- `../doc/` — the published compiler documentation and its `docs.exs` (lists the two internal_doc extracts above as `extras`).
- `../src/` — the compiler sources these notes describe (`beam_ssa*`, `ssa_opt*`, `cerl*`, `erl_parse.yrl`).
- `../test/` — test suites exercising the SSA checker and SSA passes.
