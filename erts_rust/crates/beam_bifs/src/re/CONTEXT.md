# `beam_bifs::re` — an in-crate regular-expression engine (the `re:*` BIFs)

## Intent
Implement the 8 declared `bif.tab` `re:*` keys (`version/0`, `compile/1,2`, `run/2,3`, `internal_run/4`, `inspect/2`, `import/1`) **for real** — a self-contained regular-expression engine written from scratch in safe Rust, with **no new dependency** (no PCRE, no `regex` crate; workspace licensing is Apache-2.0/MIT only).
This is the only regex engine in the workspace, and `beam_rt` does **not** shadow `re`: `default_registry()` is the dispatch path.
`lib/stdlib/src/re.erl` is almost pure Erlang on top of these NIFs, so `re:replace/3,4`, `re:split/2,3`, `re:grun/3`, `re:urun/3`, `re:compile/2` option handling and the `{capture,Spec}` → `process_parameters` machinery all work automatically once `run/2,3` and `internal_run/4` are real.
**Do not re-implement what `re.erl` does** — notably the implicit-0-prepend for a list capture spec is `re.erl`'s `process_parameters`, NOT the BIF; the BIF renders exactly the requested ids.
Ground truth (read-only): `lib/stdlib/src/re.erl`, `../../../../erts/emulator/beam/erl_bif_re.c`, `../../../../erts/emulator/pcre/pcre2_error.c` (the message table) and the live oracle `erl` on `PATH` (OTP 29 / erts-17.1).

## API Surface
The 8 keys are registered through `mod.rs`'s `pub(crate) fn register`, called from the parent family module `../runtime/other.rs`, whose `pub(crate) const REAL_BIFS` aliases `crate::re::REAL_BIFS` (the single-source roster of these keys).
- `mod.rs` — the BIF entry points, `register`, `REAL_BIFS`, `RE_VERSION`, the two compiled-pattern forms + `pattern_image`, and the shared `pattern_form`/`flatten` iodata walk + `compile_error` renderer.
- `options.rs` — the compile- and run-option parser (`parse`) and its types (`CompileFlags`, `Parsed`, `CaptureSpec`/`CapValue`/`CapType`/`CapId`, `Newline`, `Bsr`).
- `syntax.rs` — pattern codepoints → AST, plus the exact `{Msg, Pos}` compile errors.
- `prog.rs` — AST → the compiled backtracking program + the self-contained, versioned `Payload` image (encode/decode, `MAGIC = b"rustre2\0"`, `MAX_REPEAT_UNROLL = 4096`).
- `exec.rs` — the iterative backtracking matcher, the single-shot search and the `global` loop.
- `run.rs` — the `run/2,3`/`internal_run/4` bodies (option parse, RE resolve, subject flatten, start offset, result shaping).
- `render.rs` — capture ids + matcher spans → the `{match, …}` / bare `match` result terms.
- `oracle.rs` — `#[cfg(test)]`, the live-oracle differential sweep.
Every file carries the OTP Apache-2.0 `// %CopyrightBegin% … // %CopyrightEnd%` header block; the compiler-side files are long only for their `#[cfg(test)]` suites.
`re:version/0` returns `<<"10.47 2025-10-21">>` — the **compatibility-target version string of the modelled `re` surface** (what the reference oracle reports), *not* an assertion that PCRE2 is present.

## The compiled pattern term (`Mp`)
`{re_pattern, NCap, UnicodeFlag, CrlfFlag, Payload}`:
- `NCap` = the integer count of capturing groups excluding the whole match (`re:compile("(a)(?:b)(c)")` → 2, `[no_auto_capture]` → 0, a three-branch named alternation → 3).
- `UnicodeFlag` = `1` iff the `unicode` compile option, else `0`; `CrlfFlag` = `1` iff `{newline,crlf}` (the oracle's `use_crlf` = `crlf|anycrlf|any`), else `0`. `re.erl`'s `check_for_unicode`/`check_for_crlf` pattern-match elements 3/4 as integers.
- `Payload` = **our own** self-contained, versioned binary image (8-byte magic `b"rustre2\0"`): the instruction stream, the character-class table, `ncap`, the name table, the match-relevant compile flags and the raw pattern bytes, so `run/2,3` and `inspect/2` need **no** process-global registry. It round-trips our own `compile → run` and `compile → inspect`; the oracle's element 5 is a PCRE2 `#Ref<…>` and is never reproduced. `Payload::decode` is total — a truncated, foreign or version-mismatched image is a typed `badarg`, never a panic.
- `export` → `{ok, {re_exported_pattern, Header, Pattern, Options, Payload}}` where `Header` is the image magic and `Pattern` the original pattern term; `import/1` rebuilds the `{re_pattern, …}` form **around the same image** with no recompilation (the documented purpose of the form).

## Capture semantics
Default capture for `run/2,3` **and** `internal_run/4` is `{capture, all, index}`.
- non-global → `{match, [cap0, cap1, …]}` (flat, group 0 first); `global` → `{match, [[cap0,…], …]}`.
- no match → the bare atom `nomatch`; `{capture,none}`, the empty list `[]`, or an `all_names` that resolved to no names → the bare atom `match`.
- value specs: `all` (group 0 + every group), `all_but_first` (every group except 0), `first` (group 0), `none`, `all_names` (the *named* groups in name-table order, group 0 excluded), and an explicit `[ids…]` list where each id is an integer index or a name (string/atom).
- a non-participating group, a requested name no group carries, or an out-of-range index → the type's empty form `{-1,0}` (index) / `[]` (list) / `<<>>` (binary) — never an error.
- types: `index` (default) `{Start,Len}` byte offsets into the flat subject; `list` a charlist (Unicode-decoded when the pattern is `unicode`); `binary` a binary slice.

## Options
Compile: `unicode`, `anchored`, `caseless`, `dotall`, `multiline`, `extended`, `ungreedy`, `dollar_endonly`, `firstline`, `no_auto_capture`, `dupnames`, `{newline, cr|lf|crlf|nul|anycrlf|any}`, `export`, `never_utf`, `ucp`, `bsr_anycrlf`, `bsr_unicode`, `no_start_optimize`.
`no_start_optimize` and `bsr_*` are accepted no-ops (behaviour-preserving: this engine has no start optimisation, and `bsr_*` only affects `\R`, itself a typed gap); `never_utf` interacts only with `unicode` (that combination raises the oracle's `E_UTF_DISABLED` compile error); `ucp` is the typed `unsupported_bif` (see the gaps).
Run: `global`, `anchored`, `notbol`, `noteol`, `notempty`, `notempty_atstart`, `report_errors`, `{offset,N}`, `{match_limit,N}`, `{match_limit_recursion,N}`, `{capture,Spec}`, `{capture,Spec,Type}`, plus every compile option on the textual path. `{match_limit_recursion,N}` is accepted and ignored (this engine is not recursive); `export` in `run` is `badarg` (`PARSE_FLAG_EXPORT`), and a compile-only option alongside a precompiled `RE` is `badarg` (`PARSE_FLAG_UNIQUE_COMPILE_OPT`).
Pattern syntax: literals; `.`; classes `[...]` (ranges, negation, `\d\D\w\W\s\S\h\H\v\V`, POSIX `[[:name:]]`, `\Q…\E`); anchors `^ $ \A \z \Z \b \B \G`; alternation; capturing / non-capturing / named groups; atomic groups `(?>…)`; backreferences `\1`, `\g{…}`, `\k<name>`; every quantifier form `* + ? {n} {n,} {n,m}` in greedy, lazy and **possessive** form; `\N` (any-but-newline); inline and scoped option groups `(?i)`/`(?i:…)`/`(?s)`/`(?m)`/`(?x)`/`(?U)`; `(?#…)` comments; the escapes `\n \r \t \f \a \e \0 \xHH \x{…} \o{…} \cX \ddd`; `unicode` mode (UTF-8 patterns and subjects).
`caseless` folding is a simple-fold canonicaliser over ASCII, Latin-1 Supplement, Latin Extended-A, Greek and Cyrillic; a codepoint outside those ranges folds to itself — a documented coverage limit, not a silent mis-match for the covered set.

## Error behaviour
- a **malformed pattern** in `compile` → the `{error, {Msg, Pos}}` **value** (0-based byte position), with the message text taken verbatim from `pcre2_error.c`.
- `run`/`internal_run` with `report_errors`: a malformed pattern → `{error, {compile, {Msg,Pos}}}`; a match-limit exhaustion → `{error, match_limit}`. Without `report_errors`: a malformed pattern raises `badarg`; an exhausted budget is the bare `nomatch`.
- an argument the oracle refuses (a non-iodata pattern/subject, a non-`mp()` `RE`, a malformed option list, a negative `{offset|match_limit|match_limit_recursion,_}`, a non-boolean `internal_run/4` fourth argument) → `badarg`.
- `{match_limit,N}` counts **our own dispatched instructions** (default `DEFAULT_MATCH_LIMIT`), not PCRE2's internal `match()` calls, so for a small `N` the give-up threshold differs from PCRE2's; `{match_limit,0}` is oracle-exact (the budget exhausts on the first dispatched instruction).

## Constraints
- The crate is `#![deny(unsafe_code)]` — **no `unsafe`** here.
- Errors are `beam_common::{Result, BeamError}` only (`crate::error::badarg()`/`unsupported_bif()`); no crate-local error type, no `panic!`/`unwrap()` on runtime data.
- Never a wrong result: an unsupported pattern construct, option or subject outside the implemented subset raises the typed `unsupported_bif` — never a silently wrong match and never a fabricated `{Msg,Pos}`.
- Term construction: `BifContext` has `int`/`atom`/`intern_atom`/`tuple`/`cons`/`list`/`make_float`/`boolean`/`ok` but **no** binary/map constructor — binary terms are built through `crate::bits::make_binary`; term inspection goes through the crate-private `crate::access`/`crate::bits`.
- Allocation stays on `beam_alloc::Heap` via `BifContext`; the engine's own working state is plain Rust (`Vec`/`String`) and it never allocates a term on the process heap by itself.
- The matcher is fully iterative (an explicit choice-point stack, undo log and mark stack), so a 20 000-codepoint subject cannot overflow the native stack.

## Test Strategy
- In-module `#[cfg(test)]` unit tests per file (parser shapes, option parsing, matcher spans, payload round-trip, error positions) — no oracle needed; `mod.rs` is roughly half tests.
- The crate-level `../../tests/re.rs` (owned by the crate root, outside this node) pins the committed goldens over the **public** seam (`default_registry().call("re", …)`), so they run with no oracle present, and adds a live cross-check that skips loudly when no `erl` is on `PATH`.
- `oracle.rs` is this node's **live-oracle differential sweep**: 136 cases across 6 corpora (capture shapes / `global` / `{offset,N}`; the option flags; compile-error fidelity; pattern syntax; subjects and `unicode` mode; `version`/`inspect`/precompiled/`export`-`import`). Each case runs twice — under the real `erl` and through the crate's dispatch — and the two must agree on the **ETF bytes** of the result (printer-free, byte-exact); the opaque `Payload` is never compared (it is ours) and the skip-loudly policy applies with no oracle.
- Command: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_bifs --no-fail-fast`.

## Known Issues / honest typed gaps
Each gap below raises the typed `unsupported_bif` (never a fabricated value), with its blocking reason.
- **`ucp`, and therefore `\p{…}`/`\P{…}`** — the engine has no Unicode property tables, and every `ucp` pattern switches `\d\w\s` and the POSIX classes onto them; a Latin-1-only class would silently under-match, so the whole pattern is the gap.
- **Lookahead/lookbehind `(?=…) (?!…) (?<=…) (?<!…)`** — the backtracking program has no zero-width sub-match instruction.
- **`(?|…)` branch reset and `(?(…))` conditionals** — no group-number rebasing and no conditional instruction.
- **`\K`, `\R`, `\X`, `\C` and `\N{…}`** (plain `\N` IS supported) — no reset/`\R`-newline-sequence/`\X`-grapheme/`\C`-code-unit/`\N{U+…}`-named-codepoint instruction.
- **Recursion (`(?R)`, `(?1)`, `(?&name)`), callouts, `(*VERB)`, `(?(DEFINE)…)`** — not parsed.
- **A repeat bound above `MAX_REPEAT_UNROLL` (4096)** — both bounded quantifier forms unroll, so a larger bound is a documented capability gap rather than a memory blow-up (the oracle compiles `a{65535}`).
- **`re:inspect/2`'s items other than `namelist`** (`size`, `options`, `capturecount`, `newline`, …) — those describe PCRE2's own compiled blob, which this crate does not have; note the error-**class** difference: the oracle's `re.erl` raises `{error,badarg}` for the items it does not implement, while this node raises the typed `unsupported_bif`.
- **`{match_limit,N}` counts our own dispatched instructions, not PCRE2's internal match calls** — the same result class with a different give-up threshold (see Error behaviour).

## Handoffs
- The differential gate `../../../../tools/differential/` (sibling, out of this node's scope) does not yet cover `re`; its corpus should add an `re` group, for which `src/re/oracle.rs` is the ready-made case source. Suggested coverage: capture shapes; `global`/`{offset,N}`; the option flags; pattern syntax (possessive/lazy quantifiers, backreferences, named groups, classes, anchors); the compile-error `{Msg,Pos}` positions; `inspect(MP, namelist)`; `version/0`; and the `export`→`import`→`run` round-trip.
- `beam_rt` needs **no** seam for `re` — the engine is self-contained and the 8 keys reach the registry through the ordinary `crate::re::register` path, so the stdlib `re.erl` layer works unchanged.

## Routing Table
- `./mod.rs` -> the 8 BIF entry points + `register` + `REAL_BIFS` + the pattern forms
- `./options.rs` -> compile/run option and capture-spec parsing
- `./syntax.rs` -> pattern codepoints -> AST + the `{Msg,Pos}` compile errors
- `./prog.rs` -> AST -> program + the self-contained payload image
- `./exec.rs` -> the matcher + the `global` loop
- `./run.rs` -> the `run/2,3` / `internal_run/4` bodies
- `./render.rs` -> captures -> terms
- `./oracle.rs` -> the live-oracle differential sweep (`#[cfg(test)]`)
- `../runtime/other.rs` -> the parent family module that calls `crate::re::register` and aliases `REAL_BIFS`
- `../../CONTEXT.md` -> the crate root
