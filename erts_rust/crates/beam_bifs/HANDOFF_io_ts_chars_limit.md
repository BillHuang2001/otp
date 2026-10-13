# HANDOFF — `~ts` (`io_lib:format/3` unicode `~s`) under `{chars_limit, N}`

**Status: NOT IMPLEMENTED.** No source change was made; this file records the completed
reconnaissance (oracle pinning + algorithm derivation) so the work does not have to be
re-done. Delete this file once `~ts` is implemented and pinned by tests.

## 1. The two defects

1. **`~ts` under an active limit is the typed `unsupported_bif`** —
   `src/io.rs::render_limited_s` (returns `Err(unsupported_bif())` for `big.spec.unicode`).
   This is the *objective*.
2. **`~ts` *unlimited* is a silent WRONG RESULT** — `src/io.rs::string_arg` does not look at
   `spec.unicode` at all: it renders a binary's raw bytes as latin1 code points. The oracle
   decodes: `io_lib:format("~ts",[<<230,151,165>>],[])` ⇒ `[[26085]]`, the crate yields
   `[[230,151,165]]`. This outranks (1) — fix it first.

## 2. Pinned oracle output (OTP 29.1.1 / erts-17.1)

`U = [26085,26412,35486,26085,26412,35486]` (6 code points), probe script:
`io_lib:format(F, As, Opts)` with the offending option list inlined (see §7 for the harness).
Format: `name|format|args|opts|=> result`.

### 2.1 `~ts` with `U`, limit sweep (rule: keep `N-3` **grapheme clusters**, append `"..."`)

| `chars_limit` | result |
|---|---|
| 0,1,2,3 | `[[46,46,46]]` (`["..."]`) |
| 4 | `[[26085,46,46,46]]` |
| 5 | `[[26085,26412,46,46,46]]` |
| 6,7,8,9,10,11,100 | `[[26085,26412,35486,26085,26412,35486]]` (whole list) |

`~s` (latin1, ALREADY IMPLEMENTED — the reference behaviour) with `"abcdefghij"`:
`N<=3 → ["..."]`, `N=4 → ["a..."]`, `N=5 → ["ab..."]`, `N=6 → ["abc..."]`, `N=9 → ["abcdef..."]`,
`N>=10` → the whole string. **This matches `~ts` with the same N and codepoint-list input**, so
the existing `render_limited_s` / `limit_field` / `iolist_to_chars_limited` scaffolding is reused
unchanged for the length arithmetic.

### 2.2 UTF-8 binary argument (codepoints, NOT bytes)

- `~ts` of `<<230,151,165>>` (1 cp = 26085): **every** N in `0..=11,100` → `[[26085]]`.
- `~ts` of `<<230,151,165,230,151,165>>` (2 cps): every N → `[[26085,26085]]`.
- **So a 2-codepoint argument is never elided even at N=0** — the `limit==0` probe walks 4 more
  clusters and returns them when fewer than 4 remain (`string:length(L) < 4 -> L`).

### 2.3 Literal text around the control, and field width

- `a~tsb` with limit 5 → `[97,"...",98]`; limit 6 → `[97,[26085,46,46,46],98]`;
  limit 7 → `[97,[26085,26412,46,46,46],98]`; limit ≥ 8 → the whole text.
  (The surrounding literals consume `Other`, so the per-directive budget is `N - Other - 3`.)
- `~10ts`: limit 0..3 → `["..."]`; 4 → `[[26085,46,46,46]]`; 5 → `[[26085,26412,46,46,46]]`;
  6 → the whole list; **7..9** → `[[[32],…]]` / `[[[32,32],…]]` / `[[[32,32,32],…]]`
  (the field is `limit_field(10, N) = max(3, min(10, N)) = N`, so `N-6` pad spaces are
  prepended as `[S|chars(Pad, F-N)]`); ≥10 → `[[[[32,32],32,32],…]]` (four spaces).
- `~-10ts` (left adjust): same limited text, pad appended (`[S|chars(Pad,…)]`).
- `~10.3ts`: limit 4 → `[[[32],26085,46,46]]`; 5 → `[[[32,32],26085,26412,46]]`; 6 →
  `[[[32,32,32],26085,26412,35486]]`; ≥7 → pad + `[[32,32,32]…]` + full text (the `F > P`
  branch of `string/6`: `adjust(flat_trunc(S,P,Enc), chars(Pad,F-P), Adj)`).
- `~ts` of `[]` and of `<<>>` → `[[]]` for **every** N.

### 2.4 Non-limited `~ts` (must DECODE, not pass bytes through)

| argument | oracle result |
|---|---|
| `<<230,151,165>>` | `[[26085]]` |
| `<<230,151,165,230,151,165>>` | `[[26085,26085]]` |
| `<<"ab">>` | `[[97,98]]` |
| `<<200,201>>` (invalid UTF-8) | `[[200,201]]` (falls back to `binary_to_list/1`) |
| `<<237,160,128>>` (CESU-8 lone surrogate) | `[[237,160,128]]` (invalid ⇒ byte fallback) |
| `<<240,159,152,128>>` (U+1F600) | `[[128512]]` |
| `[[<<230,151,165>>,100]]` | `[[[[26085],100]]]` (inner binary decoded, **nested**) |
| `[[<<"ab">>,[99,100]]]` | `[[[[97,98],[99,100]]]]` |
| `[[26085,26412,35486]]` | `[[[26085,26412,35486]]]` |
| `[[400]]` | `[[[400]]]` |
| `[[1114112]]` | `[[[1114112]]]` |
| `[[55296]]` (surrogate as an INTEGER) | `[[[55296]]]` |
| `[[1267650600228229401496703205376]]` (bignum) | the bignum, echoed |
| `[[[]]]` | `[[[[]]]]` |
| `abc` (atom) | `[97,98,99]` (**flat** — a "small" control) |
| `97` (integer) | `badarg` |
| `{a,b}` | `badarg` |

### 2.5 `badarg` cases (never fabricate a value)

Non-limited: `[[97,-1,98]]` (negative int), `[[97,<bignum>,98]]`, `[[1.5]]` (float),
`[[97|98]]` (improper tail), `<<1:3>>` (non-byte-aligned bitstring), `~ts` of an integer or a
tuple.
**These raisers are `function_clause`/`error` inside `io_lib_format:fwrite/3`; `io_lib:format/3`
wraps them all into `error:badarg` with the 2-argument reason.**

### 2.6 Grapheme clustering IS observable (`string:next_grapheme`)

Already-verified `string:next_grapheme/1` results:
`[97,769,98]` → `[[97,769],98]`; `[[97,769],98]` → `[[97,769],98]`;
`<<97,204,129,98>>` → `[[97,769]|<<"b">>]` (improper tail); `<<230,151,165,226,130,172>>` →
`[26085|<<226,130,172>>]`; `[]`→`[]`; `<<>>`→`[]`; `[<<"abc">>,100]` → `[97,<<"bc">>,100]`;
`[255,254]` → `[255,254]` (flat); `<<200,201>>` → `{error,<<"ÈÉ">>}`.
Consequently `~ts` of `[97,769,98,99]` and of `<<97,204,129,98,99>>` counts **3** clusters, not 4
(verified for limits 4..7).

## 3. The exact oracle algorithm (`lib/stdlib/src/io_lib_format.erl`, read-only reference)

```erlang
control_limited($s, [L0], F, Adj, P, Pad, Enc, _Str, _Ord, CL, _I) ->      %% line 537
    if Enc =:= latin1  -> L = iolist_to_chars(L0, F, CL),
                          string(L, limit_field(F, CL), Adj, P, Pad, Enc);
       Enc =:= unicode -> L = cdata_to_chars(L0, F, CL),
                          uniconv(string(L, limit_field(F, CL), Adj, P, Pad, Enc))
    end;
uniconv(C) -> C.                       %% line 588 — UNICODE_AS_BINARIES is NOT defined
cdata_to_chars(Cs, F, CharLimit) when CharLimit < 0; CharLimit >= F -> cdata_to_chars(Cs);  %% 827
cdata_to_chars(Cs, _, CharLimit) -> limit_cdata_to_chars(Cs, sub(CharLimit,3), normal).
cdata_to_chars([C|Cs]) when is_integer(C), C >= $\000 -> [C | cdata_to_chars(Cs)];
cdata_to_chars([I|Cs]) -> [cdata_to_chars(I) | cdata_to_chars(Cs)];
cdata_to_chars([]) -> [];
cdata_to_chars(B) when is_binary(B) ->
    try unicode:characters_to_list(B) of L when is_list(L) -> L;
        _ -> binary_to_list(B) catch _:_ -> binary_to_list(B) end.
limit_cdata_to_chars(Cs, 0, normal) ->                                     %% 847
    L = limit_cdata_to_chars(Cs, 4, final),
    case string:length(L) of N when N < 4 -> L; 4 -> "..." end;
limit_cdata_to_chars(_Cs, 0, final) -> [];
limit_cdata_to_chars(Cs, Limit, Mode) ->
    case string:next_grapheme(Cs) of
        {error, <<C,Cs1/binary>>} -> [C | limit_cdata_to_chars(Cs1, Limit-1, Mode)];
        {error, [C|Cs1]}          -> [C | limit_cdata_to_chars(Cs1, Limit-1, Mode)];
        []                        -> [];
        [GC|Cs1]                  -> [GC | limit_cdata_to_chars(Cs1, Limit-1, Mode)]
    end.
limit_field(F, CL) when CL < 0; F =:= none -> F;                           %% 951
limit_field(F, CL) -> max(3, min(F, CL)).
sub(T,_) when T < 0 -> T; sub(T,E) when T >= E -> T-E; sub(_,_) -> 0.      %% 1110
```

Notes that cost real time to rediscover:
* `~ts` of a **non-atom** is always a `not_small` control, so `build/2` (line 104) always runs
  `build_limited/5` — **the "unlimited" path is `CL = -1`**, i.e. `control_limited($s, …, -1, …)`
  with `MaxChars = -1`, which takes the `cdata_to_chars/1` (plain) branch. It is *not*
  `control_small`.
* **`cdata_to_chars/3` and `iolist_to_chars/3` share the heading rule**: the plain branch is taken
  when `CharsLimit < 0` *or* `CharsLimit >= F`. With no width `F` is the atom `none`, and an
  integer is ordered before an atom, so `CharsLimit >= none` is always false — i.e. **a missing
  width without a negative limit always takes the limited walk**. This is exactly the existing
  `iolist_to_chars_limited`'s `plain` predicate (`src/io.rs:793`) — reuse it verbatim.
* `cdata_to_chars/1` accepts **any** non-negative integer (no upper bound: `400`, `1114112` and
  a bignum all pass); it is `limit_cdata_to_chars`'s `string:next_grapheme` that raises for them
  (via `unicode_util:gc/1`'s `error({badarg,CP})`), hence the "badarg at every N" behaviour.
* `string:length/1` (not `iolist_size/1`) counts the clusters, so an astral code point is 1.
* `io_lib:format/3` (`lib/stdlib/src/io_lib.erl:358-366`) wraps **every** raiser into
  `error:badarg` with the 2-argument reason, so `function_clause`/`badarg`/`{badarg,CP}` are all
  observably identical.

## 4. Grapheme-break machinery — full, exact source located (do NOT re-derive by sweeping)

`string:next_grapheme(CD) -> unicode_util:gc(CD)` (`lib/stdlib/src/string.erl:1122`).
`unicode_util.erl` is **generated**: `lib/stdlib/uc_spec/gen_unicode_mod.escript`, function
**`gen_gc/2` at line 787** emits the whole `gc*` family as literal text, with the per-code-point
clauses generated from the property maps. The generator's own templates are therefore the
specification, and they are ~100 lines of real logic plus generated tables:

* `cp/1` + `cpl/2` + `cpl_cont{,_2,_3}/2` + `cpl_1_cont{,_2,_3}/1` + `cp_no_bin{,l}/1,2`
  (`gen_cp/1`, line 708) — chardata normalisation, preserving improper tails and binary tails.
* `gc/1` (line 791): `\r\n` no-break, an **ASCII fast path** that returns the rest of the
  (flat) list/binary verbatim, then `gc_1/2`.
* `gc_1/2` (`gen_gc`, lines 831-903), clause order matters:
  `$\r` → *control* (`cr ∪ lf ∪ control` from GraphemeBreakProperty.txt) →
  *extended_pictographic below 256* → `is_integer(CP,0,255)` (byte-tail fast path) →
  `not IS_CP` ⇒ `error({badarg,CP})` → the rest of *control* → *prepend* → Hangul `l` → `v` →
  `t` → `44000..56000 ⇒ gc_h_lv_lvt` → *regional_indicator* → default
  (`is_ext_pict/1` ⇒ `gc_ext_pict`, else `is_indic_consonant/1` ⇒ `gc_indic`, else `gc_extend`).
* `gc_prepend/2`, `gc_extend/3`, `gc_extend2/3`, `gc_ext_pict/2,3`, `gc_ext_pict_zwj/3`,
  `gc_h_L/2`, `gc_h_V/2`, `gc_h_T/2`, `gc_h_lv_lvt/3` (from the `lv`/`lvt` maps),
  `gc_regional/2`, `gc_indic/4`, `add_acc/2`.
* Predicates, all pure per-code-point range sets: `is_control` (`cr ∪ lf ∪ control`),
  `is_extend` returning **`false | true | zwj`** (`extend ∪ spacingmark`, plus the single value of
  the `zwj` class), `is_ext_pict` (`extended_pictographic` from `emoji-data.txt`, **all** of it —
  not just the <256 prefix that `gc_1` special-cases), `is_indic_consonant`
  (`consonant ∪ vowel_independent` from `IndicSyllabicCategory.txt`, plus `16#1B0B..16#1B0C`),
  `is_indic_linker` (`virama ∪ invisible_stacker`).

**The property files are IN-REPO**: `lib/stdlib/uc_spec/{GraphemeBreakProperty,emoji-data,
IndicSyllabicCategory}.txt` (Unicode 17.0 per `vendor.info`). Parse them with the generator's
`parse_properties/2` (`to_atom/1` = `list_to_atom(lowercase(trim(Str)))`, so the map keys are
`cr, lf, control, extend, spacingmark, zwj, prepend, l, v, t, lv, lvt, regional_indicator,
extended_pictographic, consonant, vowel_independent, virama, invisible_stacker`) and merge with
`merge_ranges_1(usort(List), [])` (sorted, disjoint, adjacency-merged `{Lo,Hi}` ranges — the
`split`/`optimize` variants in the generator are asserted set-equal, so the plain form is exact).

**Verification shortcut used:** the installed oracle's `unicode_util.beam` still carries abstract
code, so the generated Erlang could be dumped and diffed against a fresh generation —
`beam_lib:chunks(code:lib_dir(stdlib) ++ "/ebin/unicode_util.beam", [abstract_code])`.

An earlier partial sweep (code points `0..0x10FFFF`) had established the *headline* membership
sets only — `Extend ∪ SpacingMark ∪ ZWJ` = 2619 code points in 334 runs, `Prepend` = 27 code
points in 15 runs — which is consistent with the above but is **not** sufficient on its own:
`gc_extend`/`gc_extend2` merge *runs* of extenders, `gc_h_*` merge Hangul runs, `gc_regional`
pairs regional indicators, `gc_ext_pict*` walks ZWJ-joined pictographic runs and `gc_indic`
walks virama-joined consonant runs, so the tables alone do not determine the cluster. Port
`gen_gc/2`'s control flow, do not approximate it.

## 5. Crate integration points (all in `erts_rust/crates/beam_bifs/src/io.rs`)

| line | item | change |
|---|---|---|
| 295-301 | `format_lib_3` doc: "The only control still not covered …" | update (U+21) |
| 176-185 | module doc: "**stays a typed, catchable `unsupported_bif`**" | update (U+21) |
| 541 / 584 | `render_format_limited`, the `b's'` arm | `~ts` of a non-atom is never "small"; it must always become `BuildItem::Big` (it already does for a non-atom, so only the atom arm's field rule is in question) |
| 693 / 720 | `render_limited_control` | pass `ctx` down to `render_limited_s` (it needs the heap to build decoded terms) |
| 746 | `render_limited_s` | the real work: unicode branch |
| 767 | `limit_field` | reuse unchanged |
| 788 / 862 | `iolist_to_chars_limited` / `limit_iolist_to_chars` | reuse the **shape** (source + stack + mode); a `CdataSource` sibling is needed because the walk is cluster-based, not byte-based |
| 1287 | `dispatch`, the `b's'` arm → `string_piece` | needs `spec.unicode` (see defect 2) |
| 1442-1470 | `string_piece` / `string_arg` | add a `unicode: bool` parameter; decode UTF-8 for a binary and for **binary leaves of a list** |

Reusable crate machinery: `crate::unicode::{decode_one, encode_utf8, copy_utf8_bin, ctl,
Encoding, Decode, Leftover}` (`src/unicode.rs`). `ctl(ctx, term, Encoding::Utf8)` **is**
`unicode:characters_to_list/1` and can be called on an existing binary *term* (each binary leaf
inside a list argument is already a term), returning the list or the `{error|incomplete, _, _}`
tuple — exactly the oracle's `cdata_to_chars/1` binary clause, including the `binary_to_list/1`
fallback. `decode_one` alone is **not** enough: it rejects a lone surrogate, whereas
`unicode:characters_to_list` tolerates interior surrogates and errors only where the oracle's
`try … catch` would fall back to bytes.

`Chunk::{Char, Int, List}` (`src/print.rs:245`) already models everything needed: a
`Chunk::Int` holds a raw code point (used by `~tc`) and `Chunk::flat_len` counts it as 1
character. `Chunk::List` supplies the oracle's per-element nesting, and `splice_flat` the
flattening of a "small" control.

`Spec` (`src/io.rs:402`) already has `unicode: bool` **but no `strings`/`maps_order` interference**
for `~s`; `parse_cseq` sets `spec.unicode = true` for the `t` modifier (`src/io.rs:1161`).

## 6. Test plan

Extend `erts_rust/crates/beam_bifs/tests/io_lib_format_chars_limit.rs` (954 lines) — it already
has a live-oracle harness to copy: `struct OracleCase{name,format,erl_args,limit,build}` (~:613),
`oracle_corpus()` (~:629), `oracle_eval()` (~:806) which emits one `erlang:term_to_binary/1` line
per case, and `run_erl` (~:881). **Two harness defects to fix while there:**

1. It has `assert!(lines.next().is_none(), "the oracle printed more lines than the corpus")`
   but **no non-zero-corpus assertion** — add `assert!(!cases.is_empty())` and assert the number
   of parsed lines equals `cases.len()` so an `erl`-side syntax error cannot silently "pass".
2. `resolve_oracle()` warns-and-skips when `erl` is absent; the objective asks it to
   **fail loudly**, or at minimum to assert the corpus ran.

Non-ASCII arguments must be built from integer lists in the generated `-eval` source, or from
`~w`-escaped binaries; a literal non-ASCII byte in the `-eval` string corrupts the eval. A
multi-line `-eval` string also breaks when a line boundary falls on `,'` — write probes as
`escript` files (this is how §2 was produced).

Also pin the **unlimited** `~ts` decode rows of §2.4 and the `badarg` rows of §2.5, and add a
committed-golden set so the behaviour is checked without `erl`.

No differential-gate case exists for `chars_limit`/`~ts`/`grapheme`
(`rg` over `erts_rust/tools/differential/` is empty), so no gate corpus needs updating — but a
case *should* be added once this lands, because the gate is the compatibility contract.

## 7. Reproduction recipes

Oracle probe (writes `name|format|args|opts|=> result` per case; run with the **installed**
`escript`, do not `cd`):

```bash
mkdir -p "$TMPDIR/gcb"
cat > "$TMPDIR/gcb/probe.escript" <<'EOF'
%% -*- erlang -*-
main(_) ->
    U = [26085,26412,35486,26085,26412,35486],
    Ns = [0,1,2,3,4,5,6,7,8,9,10,11,100],
    Cases = [{"ts_list","~ts",[U],[{chars_limit,N}]} || N <- Ns],
    [io:format("~s|~s|~w|~w|=> ~s~n",
               [Nm, F, As, O,
                io_lib:format("~w", [try io_lib:format(F,As,O)
                                     catch C:E -> {C,E} end])])
     || {Nm,F,As,O} <- Cases],
    ok.
EOF
escript "$TMPDIR/gcb/probe.escript"
```

Dump the generated `unicode_util.erl` for inspection:

```bash
erl -noshell -eval 'P=code:lib_dir(stdlib),
  {ok,{_,[{abstract_code,{_,Forms}}]}} =
      beam_lib:chunks(filename:join([P,"ebin","unicode_util.beam"]),[abstract_code]),
  file:write_file(os:getenv("TMPDIR")++"/gcb/unicode_util.erl",
                  erl_prettypr:format(erl_syntax:form_list(Forms))), halt().'
```

Build/test (mandatory wrapper — the sandbox `$HOME` is read-only; never `cd`):

```bash
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_bifs --all-targets
erts_rust/scripts/cargo.sh test  --manifest-path erts_rust/Cargo.toml -p beam_bifs --no-fail-fast
```
