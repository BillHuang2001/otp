# `beam_engine` Integration Test Suite (`tests/`)

## Intent
Black-box integration tests for the `beam_engine` crate: each `tests/<name>.rs` compiles as its
own crate and drives the engine through its **public** API. Hand-authored cases load committed
`.beam` fixtures from `fixtures/`, while the whole-corpus suites load **real installed OTP**
modules discovered generically at run time (`support/discovery.rs::all_otp_beams`/`all_otp_ebins`:
`otp_lib_root()` -> one entry per application directory whose `ebin` holds at least one `.beam`,
no application name hardcoded) in `stdlib_sweep.rs`, `census.rs`, `jit_real_module.rs`,
`jit_cranelift_real.rs` and `apps_lists.rs` — such a suite SKIPS LOUDLY when `erl` or the OTP
install is absent (a green run without `erl` proves nothing about corpus parity). `src/**` is
never touched from here.

## API Surface (what a test crate may rely on)
- `beam_engine::{Machine, MinimalContext, ExecutionContext, RootSet, RunOutcome, Step, Tier,
  TraceEvent, unsupported, MAX_REGISTER, MAX_FLOAT_REGISTER, ...}` and `beam_engine::terms::*`
  (tuple/cons/float/map builders and readers).
- `beam_loader::{Module, Instr, Operand, HeapNeed, Label, TestKind, load_file, ...}` and
  `beam_term::{Eterm, make_small, make_atom, NIL, ...}`.
- `support::*` — the shared host: the multi-module `ModuleHost` `ExecutionContext`, the
  shared-atom loader, OTP discovery, the local BIF floor, and term assertion helpers; plus the
  `support::probe` harness (`Probe`/`Expect`/`Verdict`/`ProbeRun`) for a row that must drive an
  instruction with valid operands.

## Constraints
- Read-only consumers: never edit `src/**` (owned elsewhere); never edit the crate-root docs.
- Failures are typed `beam_common::BeamError` values; never `unwrap()` engine runtime data;
  a harness-only invariant may panic with a message.
- Every `.rs` carries the OTP Apache-2.0 `// %CopyrightBegin% … // %CopyrightEnd%` header block;
  files target ~1000 lines. Seventeen suites are above that target, each one cohesive harness rather
  than a mix of subjects (splitting any of them is a parity-neutral refactor): `jit_cranelift.rs`
  (4298), `opcodes.rs` (2761), `jit.rs` (2701), `exceptions.rs` (1523), `engine_bench.rs` (1482),
  `census.rs` (1462), `dynamic_census.rs` (1415), `jit_cranelift_real.rs` (1423),
  `call_ext_apply.rs` (1333), `jit_compare.rs`
  (1319), `bitstrings_extra_big.rs` (1204), `records.rs` (1191), `raise_rooting.rs` (1098),
  `trace.rs` (1079), `stdlib_text.rs` (1016), `jit_linking.rs` (1009), `stdlib_exports_more.rs`
  (1002). `raise_rooting.rs`'s cohesion is spelled out in the routing table below; `census.rs`'s
  is the hand-maintained exhaustive `Instr`/`TestKind` rosters, which only make the "108 rows / 108
  distinct names" completeness assertion real if they live beside the census that consumes them.
  Three further oversized files under `tests/` are support/data halves rather than suites, each kept
  whole for one reason: `dynamic_census/catalog.rs` (1198) is the workload/fixture data table kept
  apart from its driver, `support/probe/mod.rs` (1117) is the single probe builder every probe row
  shares, and `support/bifs.rs` (1013) is the single host BIF floor every suite's `ModuleHost`
  shares.
- `dynamic_census.rs` is one instrument split by role, not by subject: that file is the driver
  (three layers, one `reset()` + boundary snapshots, the ranking / declared-raise / blocked-mechanism
  rendering, every assertion) and `dynamic_census/catalog.rs` (1198) is its data half (the named
  workload closures + the fixture entry points, each carrying its provenance comment). The catalog is
  reached by `#[path = "dynamic_census/catalog.rs"] mod catalog;` — a **directory** under `tests/` is
  not compiled as a test target, while a second top-level `.rs` would become one.
- One support module is past that target: `support/probe/mod.rs` (1117) is the single probe builder
  (atoms, mailbox, hooks, `Expect`/`Verdict`, the generated module and the run/verdict funnel) that
  every probe row in `support/probe/rows/` is written against — splitting it would duplicate the
  builder per fragment.
- Always the wrapper, never a bare `cargo`, never `cd`:
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine [--test <name>]`.

## Notes for Agents
- `beam_engine::exec_census` counts **only** through the interpreter funnel (`Machine::step_instruction`): a natively-inlined baseline-JIT instruction is invisible to it, so any run whose counts are meant to be complete must pin `Tier::Interpreter` (`run_export`/`ModuleHost::call` already are that path). Its `Snapshot::first_error` stores the error's `Display`, so a **structured raise renders as `error <structured reason>`** — the true class/reason rides `Raise::live_reason` as a term, not in the text, so never key an assertion on the raise text beyond that placeholder.
- The `dynamic_census` driver keeps its error map honest by classifying every run as a value or a **declared** raise; an error from an undeclared run is printed under `*** UNEXPECTED CENSUS ERRORS ***` and fails the test.
- The exhaustive per-suite enumeration is the crate-root `TEST_NOTES.md`; this file owns the
  per-fixture and per-stdlib-suite detail (`## Per-fixture suites`, `## Stdlib execution suites`).
- A suite needing the system toolchain (a real `.beam`, the `erl` oracle, or an `erlc` fixture
  build) must SKIP LOUDLY **only when the tool is absent** — a spawn failure
  (`Command::new("erl"/"erlc").output()` returned `Err`, no tool on PATH); a green run without
  `erl` proves nothing about corpus parity. A tool that RAN and exited non-zero is a HARNESS
  failure and must PANIC with its captured stdout/stderr, never be folded into "no oracle".
  Reference: `trace.rs::oracle_eval`.
- The `fixtures/` directory holds stand-in modules (`lists.beam`, `recv.beam`, …) that SHADOW
  stdlib when PREPENDED, so an oracle invocation must APPEND it (`erl -pz <dir>`), never prepend
  (`-pa`, which crashes the emulator at boot with an EMPTY stdout+stderr).
- `support/` is shared by every test crate; its facade re-exports the whole surface, so a
  re-export an individual crate does not use is not an unused import.
- **Term lifetime = arena lifetime** (`src/context.rs:1949-1964`): an `Eterm` is readable only while
  the `MinimalContext` that built it lives (the blocks go back to `beam_alloc`'s process-global pool
  and the next arena overwrites them). `ModuleHost` owns the one arena (`support/host.rs:58`) and every
  helper takes `host: &mut ModuleHost`, so a term returned by `ModuleHost::call`, `support::oracle`'s
  `breadth_args`/`arg_term`/`parse_expected_value`/`poison_term`/`ErrorTarget::reason_term` or the
  `assertions`/`int_list`-style builders is caller-owned and safe **as long as that host is alive** —
  build the host once per test, never inside a `for`/`while` body, and keep a second host alive
  whenever two runs' terms are compared. `Eterm`'s `PartialEq`/`Debug` are derived (raw word), so
  `assert_eq!` does not dereference while `beam_term::cmp`/`eq`, `render_term`, `terms::*`, `as_atom`
  and `atom_name` do.
- Two provenance-free `Eterm` carriers exist in the shared helpers and are why the rule needs stating:
  `Probe::resume_message` (`support/probe/mod.rs:615`, built outside the run's context, pushed into
  the mailbox `run` hands the engine; no row sets it) and `Observed::value()`
  (`support/probe/mod.rs:551`, documents "only valid while the `ProbeRun` is alive" but not in the
  type; no caller). `Expected::Term` (`support/oracle.rs:165`) is likewise parsed against one host and
  must be `matches`-ed against that same host. `ProbeRun` itself is the *correct* shape: it owns
  `module` + `machine` + `ctx` beside the terms.

## Per-fixture suites
Produced by the system `erlc`, the differential oracle — see `tests/fixtures/README.md` for the regeneration recipe and the opcode coverage of each fixture:
- `tests/tuples_lists.rs` — `fixtures/{tuples,lists}.beam` (`put_tuple2`/`get_tuple_element`/`select_tuple_arity`, `put_list`/`get_list`/`get_hd`/`get_tl`) plus synthetic modules for the failure paths — including `select_tuple_arity` on a **non-tuple** source, which takes the `fail` branch (`select_tuple_arity_on_a_non_tuple_branches_to_the_fail_label` over `NIL`/small-int/atom sources; `select_tuple_arity_on_a_non_tuple_without_a_fail_label_raises_badmatch` for the exhaustive `NO_FAIL` switch), matching C's `get_arity` (a non-tuple has no arity, so it behaves like an arity that matches no destination). Synthetic modules also drive the destructive `set_tuple_element` end to end (`put_tuple2` → `set_tuple_element` → `get_tuple_element` on the **same** tuple): plus its three `set_tuple_element` cases (read-back value, offset 0 = the first **data** word, neighbours and arity surviving the in-place store).
- `tests/exceptions.rs` — `fixtures/{exceptions,stacktrace,excpaths}.beam` (`try`/`catch`/`try_case`/`try_end`/`catch_end`/`badmatch`/`case_end`/`try_case_end`/`if_end`, nested `try`, a cross-frame unwind, `build_stacktrace`/`raw_raise` over the `stacktrace` fixture — the dispatch-loop test `build_stacktrace_cooks_the_raise_snapshot_innermost_first` pins that the cooked non-`NIL` trace names an **already-unwound** caller, i.e. it comes from the raise-time snapshot, not the live stack — and the **shared-atom host** section over `excpaths.beam` — a fixture whose own atom chunk carries none of the well-known exception atoms, so only a host that seeds them into every loaded module can *name* the raise reasons). The **caught-path** suite pins that `build_stacktrace` cooks the raise snapshot for a `NIL` raw `x(0)` when a raise was recorded; `the_caught_stacktrace_names_the_raising_function_and_its_live_callers` (fixture `catchtrace.beam`: `run/0` = an inline `erlang:error(badarg)`, `run2/0` = a non-tail `raise_badarg/0`) pins a real `try … catch C:R:S -> S` naming the raising function first then the live callers, live-oracle-cross-checked.
- `tests/floats.rs` — `fixtures/floats.beam` (`fmove`/`fconv`/`fadd`/`fsub`/`fmul`/`fdiv`/`fnegate`, float literals, FR temporaries, the `badarith` cases incl. the bignum→`f64` conversion and its over-range `badarith`).
- `tests/funs.rs` — `fixtures/funs.beam` (`make_fun3` with/without free vars, `call_fun`, `call_fun2`, `apply`, `apply_last`), driven through the context's **apply hook** and BIF hook.
- `tests/maps.rs` — `fixtures/mapfx.beam` (`get_map_elements`/`has_map_fields`/`put_map_assoc`/`put_map_exact`, incl. the `{badkey, a}` case and construction past the 32-key flat/hashmap boundary) plus hand-built frozen-layout HAMT terms for the hashmap read path.
- `tests/records.rs` — `fixtures/records.beam` (`update_record` under the `reuse`/`copy`/`inplace` hints, `dst` ≠ src and dst-aliasing, the `badrecord` raise, the `is_any_native_record` guard) plus synthetic modules for the failure paths (non-tuple source, size/arity mismatch, offset 0 / out of range). `Instr::PutRecord`'s **create** form (`Src = nil`) is implemented for **both** `Id` shapes C distinguishes — a bare atom is the local opcode `i_create_local_native_record` (`emu/instrs.tab:1358`, no export requirement, definition from the running module's own `Recs` table `beam_loader::Module::records`), a `{Module,Name}` pair is the generic `i_create_native_record` (`:1373`) gated on `is_exported` (`erl_record.c:401`) and resolved through the host's `ExecutionContext::foreign_record_definition` (`Ok(None)` ⇒ the typed `unsupported`; see the `records` unit entry above). `create_accepts_both_id_shapes_for_an_exported_own_record` pins the exported case (both shapes build the same record) and `create_rejects_a_non_exported_own_record_on_the_module_name_pair_form` pins the divergence (the atom still creates, the pair raises `error:{badrecord, Id}`). Native records are disabled in the installed toolchain, so this is structurally verified, **not** oracle-verified.
- `tests/bitstrings.rs` — `fixtures/bitstrings.beam` (`bs_create_bin` integer/binary/string segments, the match side, the position ops).
- `tests/bitstrings_extra_create.rs` — `fixtures/bscreate.beam` (`bs_create_bin` float 16/32/64 BE+LE, utf8/utf16/utf32 segments, and the invalid-size / non-scalar-code-point rejections).
- `tests/bitstrings_extra_match.rs` — `fixtures/bsmatch.beam` (`bs_match_string`, the `=:=` command, `bs_get_float2` 16/32/64 BE+LE, the UTF reads/skips + their malformed-input failures).
- `tests/bitstrings_extra_big.rs` — `fixtures/bsbig.beam` (integer fields wider than 64 bits on both the match and the create side: 65/68/72/128/256 bits, signed/unsigned, BE/LE) plus `fixtures/bssmall.beam` (the at-most-64-bit fields whose value can still exceed `MAX_SMALL`).
- `tests/bitstrings_extra_string.rs` — `fixtures/bsstring.beam` (`bs_create_bin` **`string`** segments: prefix/suffix/duplicate/empty literals, a string written at a non-byte-aligned bit offset, and a string after a 12-bit tail).
- `tests/bitstrings_create_fail.rs` — the `{Class, Reason}` pair a **caught** `bs_create_bin` construction failure produces, over the committed `fixtures/bsfailseg.beam` (its atom chunk carries none of the well-known atoms, so the shared-atom host must seed them), pinned end to end against the real `erl`.
- `tests/clause_dispatch.rs` — clause dispatch on `is_nil`/`is_list`/`is_nonempty_list` (`fixtures/` real modules + synthetic modules), proving the engine is **branch-for-branch identical** to the C on every input the host can run: `edlin_context:get_context/1` and its `over_word` sibling reject every non-list poison exactly like the oracle, `nil` is a list for `is_list` but not for `is_nonempty_list`, `lists` distinguishes `nil` from a cons, and `lists:reverse/1` clause dispatch agrees over every term shape (the list-shaped `edlin` inputs that only the oracle reaches are pinned as a documented host gap).
- `tests/func_info_stacktrace.rs` — the `func_info` raise's **stacktrace frames**, end to end: a failed head clause JUMPs to the function's `func_info` label, which raises class `error` with the **bare atom** `function_clause` (call args ride in the stacktrace, never in the reason). The raise carries the frames the uncaught-raise funnel `Machine::with_uncaught_frames` attaches via `Machine::raise_stacktrace(...)` — so an uncaught `function_clause` names the **raising** function innermost (`{m,f,1}`) and, for a non-tail caller, `f/1` then `g/0`, the order C's `build_stacktrace` produces. A matching head clause returns normally (the `func_info` label is reachable only from a failed clause), and a live-oracle cross-check runs `proplists:get_value(a, undefined)` under the system `erl` and asserts the innermost raw-stacktrace item is `{proplists,get_value,3}`.
- `tests/raise_stacktrace.rs` — the uncaught-raise **`Raise.stacktrace` frames** end to end, over real `erlc`-built fixtures and the `MinimalContext` cross-module/closure seams: `an_uncaught_error_raise_names_the_raising_function_then_its_caller` (a non-tail `call` into the raising `boom/1`), `a_raise_inside_a_cross_module_callee_names_the_callee_then_the_caller` (an in-machine `call_ext` via `resolve_external_call`, each frame named against its own module context), `a_raise_inside_a_closure_names_the_lambda_then_the_caller` (a `call_fun` body via `resolve_local_fun`), `the_baseline_tier_derives_the_same_frames_as_the_interpreter` (tier equivalence, frames `==` including source locations), and `an_uncaught_bare_bif_error_carries_no_stacktrace` (the documented gap — a bare `BeamError::BadArg` from a BIF with no catch frame carries no frames, while a `Raise` from the same site is decorated). Two live-oracle cross-checks run the system `erl` and skip loudly when it is absent. Note: this fixture dir shadows stdlib stand-ins, so the oracle helper must use `-pz` (append), not `-pa` (same policy as `tests/trace.rs`).
- `tests/no_fail.rs` — the `NO_FAIL` (`fail == 0`) guard semantics, hand-built synthetic modules driven through `Machine::run_label`: the four guard instructions RAISE class `error` with the **bare atom** `function_clause` from the running module's own atom table — a failed `test`, a `select_val` that matches no pair, and a missing key in `get_map_elements`/`has_map_fields`. Each is asserted both uncaught (the typed `BeamError::Raise` carrying the live reason `Eterm`) and through a live `try` frame (the reason term in `x(1)`, the `error` class atom in `x(3)`), and a module whose table cannot name the atom is the honest typed `unsupported_opcode: <site>: no 'function_clause' atom in the module`; the `put_map_assoc`/`put_map_exact` write path raises `{badmap,Source}`/`{badkey,Key}`.
- `tests/exceptions_corpus.rs` — the **corpus error-path sweep**: the error-path analogue of `tests/stdlib_sweep.rs`, over the *real* installed `stdlib`/`kernel`/`compiler` modules, proving the in-VM raise opcodes execute on real compiler output and produce **the reason term the system `erl` produces** (`function_clause`/`case_clause`/`if_clause`/`badmatch`/`badrecord`/… resolved from the host's seeded atoms); its `without_the_atom_seed_…` counterpart pins what happens when the runtime's global interning is absent.

## Stdlib execution suites
Load **real system stdlib/kernel/compiler `.beam` modules** through the shared `support` host and assert exact oracle results; a target whose module still needs an unserved NIF is recorded as a **blocked mechanism** with its exact typed error, never faked.
- **The headline stdlib integration tests** (`tests/stdlib_lists.rs` + `tests/stdlib_more.rs` + the shared `tests/support/mod.rs` host, whose helper layout `tests/CONTEXT.md` owns): load **real system stdlib `.beam` modules** (located at test time from `$ERTS_STDLIB_DIR` or `erl -noshell -eval 'code:lib_dir(stdlib)'`) and assert exact oracle results. The host re-runs the loader's public pipeline (`container::parse` → `chunk::parse` → `decode::decode_stream` → `lower::build_module`) with **one shared `AtomTable`** so cross-module atom identity (`true`/`false`/`undefined`) holds, keeps **one shared term arena** (`MinimalContext`), dispatches `call_ext*` from `call_bif` (local BIF floor → external module load + `run_export`), and resolves `call_fun`/`call_fun2` to the defining module through `apply_fun` (fun `uniq`+`index` → `Module::funs()`, entry run with arguments first and free variables after). `stdlib_lists.rs` proves `lists:reverse/1`, `lists:seq/2`, `lists:map/2`, `lists:sort/1`, `lists:filter/2`; `ordsets:from_list/union/intersection/subtract/is_element`; `proplists:get_value/2,3`, `proplists:lookup_all/2`, `proplists:get_bool/2`; plus the committed `stdlists_harness` fixture's `reverse/1`, `seq/2`, `map_double/1`, `sort/1`, `filter_pos/1` (the last two build an anonymous fun and drive it through `lists:map`/`lists:filter` — the cross-module closure path). `stdlib_more.rs` adds real `orddict`, `queue`, `gb_trees`, `gb_sets`, `ordsets` and further `lists`/`proplists` functions through the `stdlib_more_harness` fixture. The BIF floor those modules needed: the arithmetic/comparison ops, `element/2`, `tuple_size/1`, `length/1`, `++/2`, `lists:reverse/2` (a NIF in `lists.beam`), `error/1..3`/`nif_error/1` (typed `Err`); `stdlib_more` adds `erlang:'--'/2`, `bsl/2`, `bsr/2`, `max/2`, `min/2` and `lists:member/2`. The floor also serves the `erlang:is_*` type-test guards, `erlang:integer_to_list/1`, `erlang:list_to_atom/1` and `lists:keyfind/3`, and refuses `persistent_term:get/1,2` with a typed `unsupported` that **names** the missing capability (`PERSISTENT_TERM_CAPABILITY`, the unloaded NIF library). The floor additionally serves `erlang:is_integer/3` (the compiled range guard) and `erlang:raise/3` (the raise BIF the compiler lowers `throw(X)` to — see `gb_sets:is_equal_1/2`), so `calendar:time_to_seconds/1` and `gb_sets:is_equal/2` on differing sets also execute end to end.
- **Widened text / encoding stdlib execution** (`tests/stdlib_text.rs`): further **real system stdlib** modules through the same shared host — the `string` case/slice/trim/token surface (`lowercase/1`/`titlecase/1`/`casefold/1`, `trim/2,3`, `slice/2,3`, `is_empty/1`, `join/2`/`tokens/2`), the `base64` bit-syntax codec (`decode/1`, `encode/2` with its `#{padding => false}` / `#{mode => urlsafe}` option maps), the `unicode` normalisers, the `io_lib` writers and char-list predicates, the reshaping `lists` exports (`sort`/`append`/`concat`/`join`/`split`/`nthtail`/`keysort`/`enumerate`/`unzip`) plus the **fun-taking** exports (`any/2`, `all/2`, `foldr/3`, `mapfoldl/3`, `search/2`, `zf/2`) and `ordsets` via the `stdlib_text_harness` fixture — the fixture drives `make_fun3` + `call_fun*` + cross-module fun resolution into the real `lists`/`ordsets` through the `ExecutionContext::apply_fun` seam. Every expected value is the **real** system `erl` result; a target whose real module still needs an unserved NIF (`unicode:characters_to_list/1`, `binary:compile_pattern/1`) is recorded as a **blocked mechanism** with its exact typed error, never faked.
- **Second text half** (`tests/stdlib_text_more.rs`): the `unicode` *positive* character-property surface (`is_whitespace/1`, `is_id_start/1`, `is_id_continue/1`, `category/1`, `bin_is_7bit/1`, `bom_to_encoding/1`/`encoding_to_bom/1`, `characters_to_nfkc_list/1`/`characters_to_nfkd_list/1`), the `string` obsolete/alignment helpers (`strip/1`, `left/2`, `right/2`, `substr/2,3`, `equal/2`, `prefix/2`, `centre/2,3`, `pad/2`), `io_lib` `fwrite/2`/`nl/0`/`printable_list/1`/`printable_unicode_list/1`, the `lists` tuple-key family (`keydelete/3`, `keymember/3`, `keyreplace/4`, `keytake/3`) plus `prefix/2`/`suffix/2`/`merge/1,2`, and the n-ary `ordsets:union/1`/`intersection/1`. 
- **The real-module sweep** (`tests/stdlib_sweep.rs` + the discovery helpers in `tests/support/mod.rs`): the `sweep_loads_every_installed_beam` test loads **every** `.beam` under the installed OTP lib root — `support::otp_lib_root` → `all_otp_beams`, generic discovery with **no** allowlist — and **all 1366** `.beam` over **34** applications load, each yielding ≥1 function with a non-empty label map, through the same shared-atom host, asserted as `loaded == total`. It then executes a curated, **oracle-derived** set of pure functions: `lists` (recursion/list building + `lists:zip/2`), `calendar`, `filename`, `string`, `array`, `ordsets`, `dict` (`store`/`lookup` plus insertion past the expansion threshold — the captured-variable closure path), plus `maps:new/0` and `erl_scan:reserved_word/1`. Where a mechanism is still blocked the test asserts its exact typed error and never a wrong result (`base64:encode/1` **works** — `sweep_base64_builds_an_off_heap_binary`: a 60-byte input → the 80-byte oracle string, an off-heap refc result; `maps:from_list/1`, `maps:size/1`/`get/2`/`keys/1`/`put/3`, `unicode:characters_to_binary/1`, `unicode:characters_to_list/1`, `ets:new/2`, `code:which/1`, `io:format/2` → `BadArg`, and `code_server:call/1` → the typed `unsupported_opcode: receive_marker_reserve`).
- **Per-application execution breadth** (`tests/apps_lists.rs`): the widened oracle-checked sweep over OTP applications *beyond* `stdlib`/`kernel`/`compiler`, through a host whose search path is **every** installed application (`support::all_otp_ebins`) — `syntax_tools`/`erl_syntax`, `xmerl` (`xmerl_ucs`/`xmerl_lib`), `inets`/`httpd_util`, `edoc`/`edoc_lib`, `eunit`/`eunit_lib`, `dialyzer`/`dialyzer_utils`, `asn1`/`asn1ct_pretty_format`, `snmp` (`snmp_misc`/`snmp_pdus`) and `parsetools` (`leex`/`yecc:format_error/1`). `blocked_targets_report_their_typed_error` is the honest counterpart: targets the host cannot serve are asserted against their **exact** typed error (`asn1ct:use_legacy_types/0` → the process-dictionary `badarg`; `erl_syntax:atom_literal/1` → the `persistent_term` NIF refusal; `httpd_util:create_etag/1` → the structured `{badrecord, V}` raise).

## Per-tier real-module execution evidence (which tier runs a REAL `.beam`)
"Real module" = a committed `erlc +deterministic` fixture (`fixtures/`, or the sibling
`beam_loader/tests/fixtures/{slice,coverage}.beam`) or an installed OTP `.beam` found by
`support/discovery.rs`. `#![cfg(feature = "jit-cranelift")]` gates BOTH Cranelift files
(`jit_cranelift.rs:55`, `jit_cranelift_real.rs:78`); the feature is default-OFF.
- **`Tier::Interpreter`** — the whole corpus. `stdlib_sweep.rs` selects **no tier at all**
  (`ModuleHost::call` → `Machine::run_export`, `support/host.rs:175`), and its
  `sweep_loads_every_installed_beam` (`:189`) only **LOADS** every discovered `.beam` (floors
  `total >= 1000` / `apps >= 20` / `loaded == total`, `:261-274`) — the value assertions are
  separate `sweep_*` tests. Same interpreter-only entry for `coverage.rs` (`:277` over
  `coverage.beam`), `slice.rs`, the stdlib/apps/corpus suites, `census.rs` (structural) and
  `real_beam_variant_proofs/` (`harness.rs` uses `run_export`); `dynamic_census.rs:807` pins
  `Tier::Interpreter` deliberately (the census funnel is interpreter-only).
- **`Tier::BaselineJit`** — `jit_real_module.rs::run_target` drives each target twice on the SAME
  loaded module (interpreter, then baseline) over two committed corpora:
  `fixtures/stdlib_breadth_targets.txt` (504 calls over 171 modules; floors
  `BREADTH_COMPARISON_FLOOR` 480, `BREADTH_VALUE_FLOOR` 480, `BREADTH_MODULE_FLOOR` 171) and
  `fixtures/error_path_targets.txt` (232 poisoned calls over 215 modules, of which **223** are
  oracle-checked per tier; floors 220/200/200), and it asserts **two** things per row: (a) the two
  tiers agree with each other (same term under `beam_term::cmp`, or the same `Debug` error shape) and
  (b) **each tier** agrees with the baked oracle expectation — the breadth value is parsed back into a
  term and compared with `beam_term::cmp`, the error-path reason through `verify_error` (class `error`
  plus the oracle's reason term). It is therefore **oracle-checked on each tier**, not tier-vs-tier:
  measured `oracle-checked interpreter=504 baseline=504` (breadth, `blocked=0`) and `223/223` (error
  path, `blocked=18` = the 9 fixture-`unsupported` rows × 2 tiers), 171 / 215 modules, `skipped=0`,
  and **no row disagrees with the system oracle** — **727 oracle-checked rows on each tier**
  (`504 + 223`). All corpus machinery is the one shared `support/oracle.rs`.
  Other baseline real-module evidence: `jit.rs` (slice.beam, 9 `agree(..)` rows over
  both tiers), `jit_coverage.rs` (coverage.beam × `NATIVE_TIERS`, `:154`/`:158`),
  `jit_linking.rs`, `jit_cache.rs`, `jit_code_map.rs`, `jit_reductions.rs` (slice.beam),
  `jit_cross_module.rs` (fixtures compiled by the system `erlc` at test time; every tier in the
  roster on **all four** rows — interpreter + baseline always, Cranelift with the feature — the
  parked-and-resumed row included; `NATIVE_TIERS` `:504`/`:508`, the `run_both` funnel `:611`, the
  parked row `:749`), `stacktrace_frames.rs`
  (`TIERS = [Interpreter, BaselineJit]`, `:127`, live-`erl` prefix cross-check), and
  `the_block_allocator_engages_on_real_stdlib_modules` (7 real modules, engine-internal artifact
  assertions only). `jit_compare.rs` / `local_call_seam.rs` are **synthetic-only**.
- **`Tier::Cranelift`** — `jit_cranelift_real.rs` runs the **same full corpus** as the other two
  tiers, oracle-checked per tier: the same **504** breadth rows over **171** modules and the same
  **223** oracle-checked error-path rows over **215** modules, through the one shared `support/oracle.rs`
  machinery.
  Per row it asserts (a) interpreter vs Cranelift agreement and (b) *each* tier's outcome against the
  baked oracle value/reason (`Expected::matches`; `verify_error` requiring `Matches`); a Cranelift
  codegen failure is a counted `SKIP:` line, and a value/reason that differs from the oracle FAILS
  quoting the row's own `erl` command. Floors: breadth `480` cases / `480` oracle-checked **per
  tier** / `171` modules / `blocked == 0`; error path `220` cases / `200` oracle-checked per tier /
  `200` modules / `200` bare-atom `function_clause` agreements. Measured: `oracle-checked
  interpreter=504 cranelift=504` and `223/223` (`blocked=9`, the fixture-`unsupported` rows), 207
  `function_clause` agreements, 232 compared over 215 modules, `skipped=0`, zero rows differing from
  the oracle — the same **727 oracle-checked rows on this tier**.
  Plus a 26-row curated real-stdlib set whose expected values are the oracle's. The file serializes
  its own tests on a `JIT_SERIAL` mutex: `cranelift-jit` 0.116 maps one `mmap` per allocation and
  emits a 16-byte PLT stub per declared function whose `rel32` must reach that function's GOT slot, so
  two corpora's JIT modules alive at once can drift past the ±2 GiB `rel32` reach and the *dependency*
  panics with `TryFromIntError(NegOverflow)` in `write_plt_entry_bytes` (~50 % with the two sweeps
  parallel, never solo) — an address-space limitation of the dependency, never a tier divergence; the
  mutex is equivalent to `--test-threads=1`. `jit_cranelift.rs` runs slice.beam on all three tiers
  behind one `agree(..)` and is synthetic otherwise; `jit_coverage.rs` adds `Cranelift` to
  `NATIVE_TIERS` when the feature is on, so `coverage.beam` (funs/floats/`try`-raise/maps/bitstrings)
  is covered on it too; `jit_cross_module.rs` drives Cranelift through its single `run_both` funnel for
  the resolved cross-module call, the nested recursive chain and the dynamic `apply`, and its
  parked-and-resumed row covers the same roster as well — that row consults `NATIVE_TIERS` directly
  because it must resume **and** inspect each tier's own `Machine`, and it asserts the same callee park
  address, the same non-entry `park_b` context and the same resumed value on every tier (the hot tier
  resumes through the shared no-reset native seam, stepping per instruction at any address its retained
  artifact holds no compiled entry for, a cross-module callee's blocks included); the tier's own
  park/resume cases (the Cranelift resume, including one parked in a non-zero module context) are the
  case documented next.
- **Cranelift park/resume** (`src/jit/cranelift/tests.rs`): the hot tier **has** a resume entry point
  — `jit::baseline::run_native_resume_outcome` over the retained artifact store
  (`jit/cranelift/cache.rs`, keyed by `jit::cache::CacheKey`) — and
  `Machine::resume_outcome_with_tier(.., Tier::Cranelift)` is the router; a resumed slice enters
  compiled code at any address the artifact compiled and steps per instruction otherwise. Five
  cases, each compared **clause for clause** with `Tier::Interpreter`:
  `a_reduction_refusal_parks_the_machine_and_the_tier_resumes_it`,
  `a_receive_park_on_the_tier_resumes_with_the_interpreters_result`,
  `a_resume_at_the_reported_wait_pc_advances_by_the_interpreters_rule`,
  `a_resume_parked_in_a_non_zero_module_context_agrees_with_the_interpreter`,
  `a_resume_does_not_double_count_the_reduction_charge`; the park side is
  `a_receive_park_reports_the_interpreters_address_and_keeps_the_artifact`.
- **Gaps by tier:** all three tiers run the full corpus oracle-checked (`jit_real_module.rs` for
  interpreter+baseline; `jit_cranelift_real.rs` for interpreter+Cranelift) — 727 rows per tier.
  No suite drives `Tier::Cranelift` over `census.rs`/`dynamic_census.rs`/`real_beam_variant_proofs.rs`/
  `stdlib_sweep.rs`, and the corpora reach 171/215 of the installed modules, not all 1366.
  `stdlib_sweep.rs::sweep_loads_every_installed_beam` remains **load-only** (it executes no
  instruction): a per-module execution sample over all 1366 modules has no oracle column, because a
  module's only universally-present exports are `module_info/0,1`, which the compiler lowers to the
  `erlang:get_module_info/1,2` BIF that the test host's BIF floor does not serve — so no expectation
  could be produced reliably. The execution evidence is the separate `sweep_*` tests plus the two
  oracle-checked corpora (727 rows per tier). Values are baked at authoring time (no live `erl` per
  assertion) except `jit_cross_module.rs`'s test-time `erlc` and the live-oracle suites (`trace.rs`,
  `func_info_stacktrace.rs`, `raise_stacktrace.rs`, `stacktrace_frames.rs`).
- **`#[ignore]`d suites here:** exactly two — `stacktrace_frames.rs:710` (`#[ignore = "capability
  gap: `ModuleHost` supplies no mailbox, …"]`) and `jit_bench.rs:57` (`#[ignore = "measurement
  table: run with --ignored --nocapture"]`). Run either with
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test
  <name> -- --ignored --nocapture`; nothing Cranelift is `#[ignore]`d (feature-gated instead).

## Routing Table
- `support/` → shared multi-module `ExecutionContext` host: OTP discovery (`otp_lib_root`/`all_otp_beams`), the shared atom universe, the BIF floor, assertion helpers
- `support/oracle.rs` → the **one** implementation of the corpus machinery every oracle-checking suite uses: `parse_breadth_target`/`BreadthTarget` + `split_arg_terms`/`arg_term`/`breadth_args`, `parse_expected_value`/`Expected` (+ `matches` through `beam_term::cmp`), the `~w` renderer `render_term`, and the error-path half `parse_error_targets`/`ErrorTarget`/`ErrorVerdict`/`verify_error` (+ `Poison`/`poison_term`, `describe_error`). Re-exported **by name** from the facade (its `render_term` would collide with `probe`'s in a glob); `stdlib_lists.rs`, `exceptions_corpus.rs` and `jit_real_module.rs` all consume it — `jit_cranelift_real.rs` is the remaining consumer
- `support/probe/` → the valid-operand probe harness (`mod.rs`: `ProbeAtoms`, `ProbeMailbox`, `Expect`, `Observed`, `Verdict`, the `Probe` builder, `ProbeRun`, `render_term`, the fixed entry/aux/fail-sink labels) and its roster in `rows/` — one file per instruction group (`core.rs`, `data.rs`, `exceptions.rs`, `receive.rs`, `bitstrings.rs`, unioned by `rows/mod.rs::all_rows`), keyed by the variant name **derived** from the constructed `Instr` so a row can override a legacy row but never invent a name
- `fixtures/` → committed system-`erlc`-produced `.beam` fixtures + `.erl` sources + target lists. The real-`.beam` variant proofs own five of them: `vb_calls` (`Call`/`CallExt`/`CallLast`/`CallExtOnly`/`CallExtLast`/`ApplyLast`/`Apply` shapes), `vb_data` (tuple/list/map reads and writes), `vb_funs` (`MakeFun3`/`CallFun`/`CallFun2`), `vb_exc` (`Try`/`TryCase`/`TryCaseEnd`/`TryEnd`/`Catch`/`CatchEnd`/`IfEnd`/`BuildStacktrace`), `vb_bitstr` (the `Bs*` match/create set), each committed as `.erl` + `.beam` so a proof needs no compiler at test time
- `rooted_alloc.rs` → proves the engine's term-construction instruction arms route storage through `ExecutionContext::alloc_words_rooted` (whole-root hand-off; a counting context whose `alloc_words` stays the typed-error default so any fallback fails loudly; a moving-collector relocation proof)
- `raise_rooting.rs` → the raise-window counterpart: a hand-built `function_clause` raise driven through a semi-space **moving** mock collector (`MovingContext`, which relocates every term it is handed and poisons the region it vacated), proving the parked `raise_function_clause_args` latch rides `Machine::with_roots`'s published root set; it also covers the `raw_raise` register protocol — `a_raw_raise_reason_is_re_read_after_the_cooking_reservation` asserts a heap-**tuple** reason installed by `Instr::RawRaise` is the term the cooking reservation's collection wrote back, not the pre-reservation word. One cohesive file by design (~1430 lines): the single mock collector, the raise fixtures and every latch-survival shape (parked vs unparked hand-off, cooked vs uncaught trace, post-move assertions) share that one harness.
- `fun_seam_rooting.rs` → the closure-apply/cross-module-seam counterpart, over its own semi-space copying mock collector: a seam that **collects** (`alloc_words_rooted` reached from inside a test context that drives `Machine::with_roots` itself — `MinimalContext` never collects) proves `call_fun` publishes the *moved* free variables, `apply_fun_outcome` is handed the *moved* fun and args, and `apply_target` dispatches the *moved* argument list; a non-collecting control row pins that the values are unchanged when nothing moves.
- `bif_args_window.rs` → the **BIF argument-window** invariant: a counting context proves a narrow (`arity < 3`) and a wide (`arity >= 3`) dispatch cross **zero** rooted-allocation/heap-admission doors while the argument window is open (on both the plain `call_bif` and the in-machine `call_bif_in_machine` doors), that both hand the host exactly the live `x(0..arity)`, and — via a live `test_heap` control row — that the probe really does see a real admission point.
- `type_sizes.rs` → observes the frozen IR size guards (`Instr` ≤ 64 / `Operand` ≤ 16 / `HeapNeed` ≤ 24) over the public types (authoritative pins are the compile-time asserts in `beam_loader/src/instr.rs`)
- `census.rs` → the `Instr`-variant / `TestKind` execution census + whole-OTP corpus reachability. Four buckets (`EXECUTED` / `ZERO-CORPUS` forward-compat / `GENUINE-GAP` / `UNCLAIMED`) partition the 108-variant roster, pinned by `EXECUTED_VARIANTS` / `ZERO_CORPUS_VARIANTS` / `GENUINE_GAP_VARIANTS`; 64 rows are probe-driven with valid operands and a declared `Expect`, the other 44 keep the compact degenerate-operand module. `ZERO_CORPUS` is the 13-name forward-compat pin, asserted **equal** to the live zero-occurrence set measured over the whole installed OTP, so a variant the toolchain starts or stops emitting fails the build
- `dynamic_census.rs` + `dynamic_census/catalog.rs` → the **dynamic** per-variant ranking: one serialized test drives real compiled Erlang on `Tier::Interpreter` over three layers (the whole-OTP `module_info/0,1` sweep, the named stdlib workload catalog, the committed fixtures incl. a mailbox-driven `recv` section), attributes weight per layer from boundary snapshots of `beam_engine::exec_census`, and reports the combined ranking, the DECLARED-raise vs UNEXPECTED error split, the blocked-mechanism table, the computed not-reached list and the TSV dump. `catalog.rs` holds only the workload/fixture data
- `real_beam_variant_proofs.rs` + `real_beam_variant_proofs/` → the real-compiler-output proofs for the variants the retired probe census could only reach degenerately: one directory module split by role (`harness.rs`: the fixture driver + the live-`erl` oracle comparison + the `exec_census`-observed assertion; `calls.rs`, `data.rs`, `funs.rs`, `exceptions.rs`, `bitstrings.rs`, `receive.rs`: one proof file per instruction group), proving **41** ordering variants (plus **8** further shapes reached en route) on the five `vb_*` fixtures + the mailbox `recv` run, each row asserting `exec_census` observed the variant execute **and** that the observable result equals the live `erl` oracle, with `RECORDED_PROOFS` (49 names) checking coverage and uniqueness and `Deallocate` pinned at exactly 0
- `reductions.rs` → per-instruction reduction charging (exactly-once, host pre-emption) and the suspend/resume slice boundary
- `send_invariants.rs` → the `send` arm's two **live** invariants over hand-built synthetic modules (a `snd` `run/4` receive-then-send, a bare `run/2` send and a send inside a live `try`; no fixture `.beam`, no loader) driven through `run_export_outcome`/`resume_outcome`/`run_export` over a recording `ExecutionContext` host: a `send` in the slice a `receive` park is later resumed into delivers the `x(0)`/`x(1)` the **live** register file holds at execution time — never a pair cached before the park — and leaves the message in `x(0)` (`erl_send`'s return, `bif_instrs.tab:436-441`), while a host-side delivery failure (`BeamError::TermNotFound`) is **surfaced**, uncaught and inside a live `try`, with `x(0)` untouched and exactly one delivery attempt, never folded into a success. Pins the engine half only: the C's dead-destination drop (`bif.c:2478-2480`) is the *host's* semantics, already folded by this crate's own `call_ext erlang:send/2` fast path (`src/interpreter/dispatch.rs:992-1002`).
- `gc_roots.rs`, `gc_arena.rs`, `bif_alloc.rs` → GC root-set hand-off, the region heap, BIF-driven allocation
- `heap_reclaim.rs` → the arena's two reclaim tiers (`heap_rewind` / `reclaim_heap` / `reclaim_heap_with_capacity`) through their **public** doors at the **production** geometry (4 MiB reservation / 64 KiB commit, a second region only past a bulk request): both tiers end to end, the reachable refusal arms (foreign / pre-rewind / spent-`EMPTY` marks) with byte-identical arenas, the reset door, both `Ok(None)` arms, the pre-copy `heap_exhausted` capacity refusal, and one `Machine`-rooted forward through `Machine::for_each_root_mut`; the exact-number `ContextHeap::with_geometry` half lives in `src/context/reclaim/tests.rs`
- `map_assoc_alloc.rs` → `put_map_assoc`'s capacity count is O(1): a per-thread-armed counting `#[global_allocator]` decomposes one instruction's allocation cost into `harness + build + one source walk` (measured in-binary) and requires the instruction to fit that plus a slack smaller than a walk, so a re-introduced `map_entries(...).len()` second walk fails; the produced map is pinned against the reference builder down to `map_entries` storage order
- `jit.rs`, `jit_real_module.rs` → native baseline tier equivalence (synthetic + real-`erlc` modules; see `## Per-tier real-module execution evidence` for which tier each suite drives and the exact corpora/floors). `jit_real_module.rs` is **oracle-checked per tier** (interpreter + baseline): per corpus row it runs interpreter + baseline, asserts they agree with each other, then additionally asserts *each* tier's own outcome against the fixture's baked oracle expectation (`Expected::matches` for a breadth value, `support::verify_error` requiring `Matches` for an error-path row; the fixture-`unsupported` rows print as counted `BLOCKED:` lines and a `Wrong` panics quoting the row's `erl` command) — measured `interpreter=504 baseline=504` breadth (171 modules) / `223/223` error path (232 compared over 215 modules), 727 oracle-checked rows per tier; `jit_cross_module.rs` → the native tiers' in-machine module-context switch on `erlc`-compiled-at-test-time modules, on every tier in the roster on all four rows (interpreter + baseline always, Cranelift with the feature) — the parked-and-resumed row included, which consults `NATIVE_TIERS` directly because it resumes and inspects each tier's own `Machine` (the hot tier's resume goes through the shared no-reset native seam, with the tier's own park/resume unit cases, including one parked in a non-zero module context, in `src/jit/cranelift/tests.rs`); `jit_linking.rs`, `jit_cache.rs`, `jit_code_map.rs`, `jit_compare.rs`, `jit_coverage.rs` → block linking, the cross-run cache, the host code map, and baseline-vs-interpreter coverage; `jit_bench.rs` → the thin driver over the `crate::jit::bench` measurement harness; `jit_cranelift.rs`, `jit_cranelift_real.rs` → the feature-gated Cranelift hot tier, the second being the interpreter+Cranelift leg of the same 727-row per-tier evidence, with the tier's park/resume cases in `src/jit/cranelift/tests.rs`
- `no_fail.rs` → the integration-level `NO_FAIL`-guard evidence: drives `test` / `select_val` / `get_map_elements` / `has_map_fields` with `fail == NO_FAIL` through the real public entry `Machine::run_label` and asserts the DEFINED raise is class `error` + the bare atom `function_clause`, **both** uncaught (a `BeamError::Raise` carrying the live reason term) **and** caught in a live `try` frame (handler observes class `error` / reason `function_clause`). The positive counterpart to `gaps.rs`'s typed "no such atom" pins; complements the in-crate unit tests in `src/interpreter/tests.rs` and `src/families/maps/tests.rs`
- `stdlib_text.rs` → the text / byte-encoding / collection-list stdlib surface on the real installed stdlib: `string` (case mapping + `trim`/`slice`/`join`/`tokens`), `base64` (`decode/1`, `encode/2` with its option maps), `unicode` normalisers, the `io_lib` writers/predicates, the reshaping + fun-taking `lists` exports, and `ordsets`; targets still needing an unserved NIF (`unicode:characters_to_list/1`, `binary:compile_pattern/1`) are recorded as blocked mechanisms with their exact typed error
- `stdlib_text_more.rs` → the second text half: the `unicode` character-property surface (`is_whitespace`/`is_id_*`/`category`/`bin_is_7bit`/`bom_to_encoding`/`encoding_to_bom` + the list-only `characters_to_nfkc_list`/`nfkd_list`), the obsolete/alignment `string` helpers (`strip`/`left`/`right`/`substr`/`equal`/`prefix`/`centre`/`pad`), `io_lib` `fwrite/2`/`nl/0`/`printable_*`, the `lists` tuple-key/`prefix`/`suffix`/`merge` family, and the n-ary `ordsets` `union/1`/`intersection/1`
- `stdlib_collections.rs` → the additional collection/misc stdlib exports: `calendar` (tuple-form entry points + the second-based conversions), `filename` (with `kernel`'s `os` on the search path), `array` (`make_tuple`-built interiors + `setelement` updates), `dict` (a hash dictionary reaching captured-variable closures + `phash/2` bucketing), `orddict`
- `stdlib_collections_trees.rs` → the balanced-tree collections `gb_trees` (`from_orddict`/`is_defined`/`update`/`take*`/`smallest`/`largest`/`iterator`+`next`/`balance`) and `gb_sets` (`union`/`intersection`/`subtract`/`singleton`/`is_equal`/`take_smallest`/`take_largest`), including the set algebra
- `stdlib_collections_queue_maps.rs` → `queue` (the two-list FIFO inserters/rear accessors/rear droppers/`join`/`split`/`delete` family) and `maps` (whose map-BIF operations are asserted as a **host gap** with their exact typed error, never a wrong result)
- everything else (`stdlib_lists`, `stdlib_more`, `stdlib_sweep`, `apps_lists`, `coverage`, `opcodes`, `tuples_lists`, `maps`, `funs`, `floats`, `records`, `bitstrings*`, `exceptions*`, `receive`, `tailcall`, `external_call`, `call_ext_apply`, `cross_module_catch`, `local_call_seam`, `apply_atom_value`, `clause_dispatch`, `func_info_stacktrace`, `current_module`, `process_info`, `wait_deadline`, `trace`, `gaps`, `bignum_compiled`, `slice`, and the `*_bench`/`*_cost` measurement suites) → per-family / per-contract execution and error-surface suites; see `TEST_NOTES.md`.
