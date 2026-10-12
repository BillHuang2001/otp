# `epp:parse_file/3` on the scheduled (`run`) route — localization findings

Status: **localized, not fixed.** The invalid-form / `error:badarg` divergence is caused by
**the `epp` server process's own heap being clobbered**, not by the file-server seam, the ETF
codec, or a sibling crate's protocol handling. All instrumentation used to reach this conclusion
has been removed from the tree.

## 1. Reproduction (deterministic)

Source `x1_in.erl` (43 bytes):

```erlang
-module(x1_in).
-export([f/0]).
f() -> 1.
```

`go/0` calls `epp:parse_file(File, [], [])`, renders every returned form with
`io_lib:format("~w~n", [F])` into a file, with a `try … catch` so a render failure is recorded.

* `beam_rt -noshell run x1.beam x1:go/0` → 6 forms, of which the **`-export` form renders as
  `<<RENDER-FAIL error:badarg>>`** and an **extra `{error,{4,epp,cannot_parse}}`** appears.
* `beam_rt -noshell -pa . -eval 'x1:go()'` → the 5 oracle-identical forms, no error.

The bad form is not an Erlang term of any type (`is_tuple/is_list/is_atom/…` all `false`,
`element/1` fails, `io_lib:format("~w", …)` raises `badarg`,
`erlang:term_to_binary(F, [])` raises the codec's generic unknown-tag
`unsupported_external_term`).

Minimal sub-reproducers (same module, `run` route only):

| Probe | Shape | Outcome on `run` |
|---|---|---|
| `epp:scan_erl_form/1` in a loop | renders each token list | tokens **correct**, but an extra device error `{error,{4,epp,cannot_parse}}` where the oracle has `{eof,4}` |
| manual `epp:scan_erl_form/1` + `erl_parse:parse_form/1` | renders tokens **and** form | **everything correct** (allocation timing matters) |
| `epp:parse_erl_form/1` loop | nothing rendered between steps | epp server **dies** (`beam_rt: term not found` — a later send to a dead pid) |
| `io:scan_erl_form(F, '', N, [])` from the main process | renders results | **correct** — the native file server is fine |
| plain cross-process send of the same tokens/forms | render | **correct** — the copier is fine for these shapes |

## 2. Instrumentation that localized it

An env-gated trace was installed on (a) the nested `erl_scan:tokens/4` call in
`src/native/file_server/get_until.rs`, and (b) the delivery seam
(`RuntimeContext::deliver_message_with_off_heap_inner`) — printing the request's `XtraArgs[1]`
(the scan `Options`) as a raw word sketch, i.e. **on the sender's own term, before the copy**.

Sender-side transcript (`epp:scan_erl_form/1` loop; identical shape for `parse_file/3`), where
`Options` is `[{text_fun, F}, {reserved_word_fun, F}]`:

```
TRACE send cmd=get_until xtra1=List[0x743a875d8702, 0x743a875d8719]   # request 1 — intact
TRACE send cmd=get_until xtra1=List[0x743a875d8702, 0x743a875d8719]   # request 2 — intact
TRACE send cmd=get_until xtra1=List[0x743a875d8702, 0x743a875d8719]   # request 3 — intact
TRACE send cmd=get_until xtra1=List[0x2f, 0xf]                        # request 4 — CLOBBERED
```

`0x2f`/`0xf` are the small integers `2` and `0`: the cons cell that used to head the `Options`
list now reads `[2|0]`. The service-side trace shows exactly the same `[2|0]`, i.e. **the term
was already corrupt before it was copied**. Consequence: `erl_scan:tokens([], eof, Line, [2|0])`
matches no clause and raises `function_clause` inside the nested (native) run, so
`file_io_server` boxes it as `{error, tokens}`, which `epp:scan_toks/3` (`lib/stdlib/src/epp.erl`
lines 1117-1119) turns into `{error, {4, epp, cannot_parse}}` and an extra `leave_file/2` step.

The same clobbering is what corrupts the `{attribute,2,export,[…]}` form: the epp server holds
the per-form token list in its own state/heap before shipping it as `{ok, Toks}`; when that
region is clobbered, the caller receives a corrupted token list and `erl_parse:parse_form/1`
then yields a non-term. Rendering the token list in the caller (probe 2) shifts the allocation
pattern enough that the clobber lands elsewhere — which is why that probe looked healthy.

## 3. Conclusion

* The native `file_server` (`get_until` → `beam_io::io_device::Continuation` → `erl_scan:tokens/4`)
  is **correct**: it returns the right tokens and the right `eof`, and the same code path from the
  main scheduled process agrees with the oracle.
* The message copier is **correct** for these shapes (plain cross-process control agrees).
* The divergence is that a **scheduled process's heap stops preserving live terms in that
  process's own state** (the epp server's `St#epp.erl_scan_opts` and the in-flight token list)
  after a few request/reply rounds. That points at the scheduled-process heap-management path
  (`beam_rt`'s runner/GC glue driving `Process::grow_nursery` / the nursery chain, and/or
  `beam_process`'s nursery growth), not at `beam_rt`'s file-server, mailbox or CLI layers.

Remaining work: pin the exact nursery-growth/root-publication site and fix it there; then add a
regression test in `tests/compile_file.rs` that drives `epp:parse_file/3` (or `compile:file/2`)
through the scheduled `run` route and asserts the observable parsed form count / produced `.beam`.
