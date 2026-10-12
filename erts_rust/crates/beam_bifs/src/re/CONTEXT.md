# `beam_bifs::re` — an in-crate regular-expression engine (the `re:*` BIFs)

## Intent
Implement the 8 `re:*` BIFs (`version/0`, `compile/1,2`, `run/2,3`, `internal_run/4`, `inspect/2`, `import/1`) **for real** — a self-contained regular-expression engine written from scratch in safe Rust, with **no new dependency** (no PCRE, no `regex` crate; workspace licensing is Apache-2.0/MIT only).

This is the only place in the workspace where a regex engine exists; the sibling `tools/differential/` gate drives it through `beam_rt`, and `beam_rt` is *not* shadowing `re` — this registry is the dispatch path.

`lib/stdlib/src/re.erl` is almost pure Erlang on top of these NIFs, so `re:replace/3,4`, `re:split/2,3`, `re:grun/3`, `re:urun/3`, `re:compile/2` option handling and the `{capture,Spec}` → `process_parameters` machinery all work automatically once `run/2,3` and `internal_run/4` are real. **Do not re-implement what `re.erl` does** (notably: the *implicit-0-prepend for a list capture spec* is `re.erl`'s `process_parameters`, NOT the BIF — the BIF must render exactly the requested ids).

Ground truth (read-only): `lib/stdlib/src/re.erl` and the live oracle `erl` on `PATH` (OTP 29.1.1 / erts-17.1).

## API Surface (the seam `src/runtime/other.rs` calls)
- `mod.rs` — the BIF entry points + `pub(crate) fn register(reg: &mut BifRegistry)` + `pub(crate) const REAL_BIFS: &[(&str, &str, u32)]` (exactly the 8 keys).
- `options.rs` — compile-option and run-option parsing; `CompileOpts`, `RunOpts`, `CaptureSpec`, `CaptureType`.
- `syntax.rs` — pattern text → AST, with the oracle's `{Msg, Pos}` compile errors.
- `prog.rs` — AST → compiled program + the self-contained `Payload` term (encode/decode).
- `exec.rs` — the matcher (produces match + capture spans over the flat subject).
- `render.rs` — capture spans → the `{match, Caps}` / `nomatch` / `match` terms.

Every file carries the OTP Apache-2.0 `// %CopyrightBegin% … // %CopyrightEnd%` header block (copy it byte-exactly from `../runtime/other.rs`). Files target ~1000 lines; split rather than grow (a longer file needs a top-of-file "why this must be long" comment).

## The compiled pattern term (`Mp`) — oracle-verified shape
`{re_pattern, NCap, UnicodeFlag, CrlfFlag, Payload}`:
- `NCap` = integer count of capturing groups, excluding the whole match. `re:compile("(a)(?:b)(c)")` → element 2 = 2; `re:compile("(a)(b)",[no_auto_capture])` → 0; `re:compile("(?<A>A)|(?<B>B)|(?<C>C)")` → 3.
- `UnicodeFlag` = `1` if the `unicode` compile option else `0`. `CrlfFlag` = `1` if `{newline,crlf}` else `0` (`re.erl`'s `check_for_unicode`/`check_for_crlf` pattern-match elements 3/4 as integers).
- `Payload` = **our own self-contained** term (recommended: a custom binary image) carrying the compiled program + name table + option flags, so `run/2,3` and `inspect/2` need **no global registry**. It must round-trip through our own `compile → run` and `compile → inspect`. **Never** try to reproduce the oracle's `#Ref<…>` in element 5.
- `export` compile option → `{ok, {re_exported_pattern, NCap, UnicodeFlag, CrlfFlag, Payload'}}`, where `Payload'` also carries the source string + options; `import/1` returns the `{re_pattern,…}` form (recompile if the payload is not usable).

## Capture semantics (pin exactly)
Default capture for `run/2,3` **and** `internal_run/4` is `{capture, all, index}`.
- non-global: `{match, [cap0, cap1, …]}` (flat; group 0 = whole match first).
- `global`: `{match, [[cap0,…], [cap0,…], …]}`.
- no match → `nomatch`; `{capture,none}` → bare atom `match`; an **empty list** spec `[]` → bare atom `match`; `all_names` with no named groups → bare atom `match`.
- value specs: `all` (group 0 + every group), `all_but_first` (every group except 0), `first` (group 0 only), `none`, `all_names` (values of the *named* groups in **alphabetical name order**, group 0 NOT included), `[ids…]` where each id is an integer index or a name (string/atom). A non-participating group, or a requested name that does not exist → `{-1,0}` (index) / `[]` (list) / `<<>>` (binary).
- types: `index` (default) `{Start,Len}` byte offsets into the flat subject; `list` charlist of small ints (Unicode-decode the UTF-8 slice when unicode); `binary` binary slice.

## Options
Compile: `unicode`, `anchored`, `caseless`, `dotall`, `multiline`, `extended`, `ungreedy`, `dollar_endonly`, `firstline`, `no_auto_capture`, `dupnames`, `{newline, cr|lf|crlf|nul|anycrlf|any}`, `never_utf`, `ucp`, `bsr_anycrlf`, `bsr_unicode`, `no_start_optimize`, `export`. Accept as no-ops **only where behaviour-preserving**; otherwise the typed `unsupported_bif`. The supported subset is documented honestly at this node.
Run: `anchored`, `global`, `notbol`, `noteol`, `notempty`, `notempty_atstart`, `report_errors`, `{offset,N}`, `{match_limit,N}`, `{match_limit_recursion,N}`, `{capture,Spec}`, `{capture,Spec,Type}`, plus every compile option when `RE` is not precompiled.

Pattern syntax: literals; `.`; classes `[...]` incl. ranges, negation, `\d \D \w \W \s \S`, POSIX `[[:alpha:]]` etc.; anchors `^ $ \A \z \Z \b \B`; alternation; capturing `(...)`; non-capturing `(?:...)`; named `(?<name>...)`/`(?P<name>...)`; backreferences `\1`; quantifiers `* + ? {n} {n,} {n,m}` greedy, lazy (`*?`), possessive (`*+`); escapes `\n \t \r \f \v \0 \xHH \x{...} \uXXXX \\` + `\Q…\E`; inline option groups `(?i)`/`(?s)`/`(?m)`/`(?x)`/`(?U)` incl. scoped `(?i:…)`; the `unicode` option (patterns + subjects as Unicode charlists — reuse the crate's `src/unicode/` module).

## Error behaviour (oracle-verified)
- `compile` failure → `{error, {Msg, Pos}}` (0-based byte position). Reproduce **exactly** the messages you can; anything you cannot match exactly must raise the typed `unsupported_bif` — **never fabricate a message or a position**.
- `run`/`internal_run` with `report_errors`: compile failure → `{error, {compile, {Msg,Pos}}}`; match-limit exhaustion → `{error, match_limit}`. Without `report_errors`: compile failure raises `badarg`; match-limit exhaustion → `nomatch`.
- Non-iodata / non-`mp()` `RE`, or a non-iodata subject → `badarg`.

## Oracle goldens to pin (all verified against OTP 29.1.1 / erts-17.1)
```
re:run("xabcy","b")                                     = {match,[{2,1}]}
re:run("xabcy","(b)(c)")                                = {match,[{2,2},{2,1},{3,1}]}
re:run("xabcy","b",[{capture,none}])                    = match
re:run("xabcy","(b)(c)",[{capture,[],index}])           = match
re:run("abab","a",[global])                             = {match,[[{0,1}],[{2,1}]]}
re:run("abab","(a)(b)",[global,{capture,all,index}])    = {match,[[{0,2},{0,1},{1,1}],[{2,2},{2,1},{3,1}]]}
re:run("xabcy","(b)(c)",[{capture,all,list}])           = {match,["bc","b","c"]}
re:run("xabcy","(b)(c)",[{capture,all,binary}])         = {match,[<<"bc">>,<<"b">>,<<"c">>]}
re:run("xabcy","(b)(c)",[{capture,all_but_first,index}]) = {match,[{2,1},{3,1}]}
re:run("xabcy","(b)(c)",[{capture,[2,1],index}])        = {match,[{3,1},{2,1}]}
re:run("xabcy","(b)(c)",[{capture,[0,1],index}])        = {match,[{2,2},{2,1}]}
re:run("xabcy","(b)(c)",[{capture,first,list}])         = {match,["bc"]}
re:run("xabcy","(?:b)(c)",[{capture,all,index}])        = {match,[{2,2},{3,1}]}
re:run("xabcy","(z)?(b)",[{capture,all,index}])         = {match,[{2,1},{-1,0},{2,1}]}
re:run("xabcy","(z)?(b)",[{capture,all,list}])          = {match,["b",[],"b"]}
re:run("xabcy","(z)?(b)",[{capture,all,binary}])        = {match,[<<"b">>,<<>>,<<"b">>]}
re:run("xabcy","(?<foo>b)(c)",[{capture,all_names,index}]) = {match,[{2,1}]}
re:run("xabcy","(?<foo>b)(c)",[{capture,["foo"],index}])   = {match,[{2,1}]}
re:run("xabcy","(?<foo>b)(c)",[{capture,["c"],index}])     = {match,[{-1,0}]}
re:run("abcabc","b",[{offset,2}])                       = {match,[{4,1}]}
re:run("ABC","abc",[caseless])                          = {match,[{0,3}]}
re:run("abc",".")                                       = {match,[{0,1}]} ; re:run("a.c",".",[dotall]) = {match,[{0,1}]}
re:run("a\nb","b$",[multiline])                         = {match,[{2,1}]}
re:run("a\nb","(?s)a.b")                                = {match,[{0,3}]}
re:run("abc","a  b  c",[extended])                      = {match,[{0,3}]}
re:run("abc","\\Aabc") / re:run("abc","bc\\z")          = {match,[{0,3}]} / {match,[{1,2}]}
re:run("abc","\\bb")                                    = nomatch
re:run("abc","[a-c]+") / re:run("abc1","\\d+") / re:run("abc_1","\\w+") / re:run("a b","\\s")
                                                        = {match,[{0,3}]} / {match,[{3,1}]} / {match,[{0,5}]} / {match,[{1,1}]}
re:run("abab","(a)(b)\\1")                              = {match,[{0,3},{0,1},{1,1}]}
re:run("aaa","a{2,}") / re:run("aaaa","a{2,3}") / re:run("aaa","a+?")
                                                        = {match,[{0,3}]} / {match,[{0,3}]} / {match,[{0,1}]}
re:run("ABC","(?i)abc") / re:run("abc","a(?i:BC)")      = {match,[{0,3}]} / {match,[{0,3}]}
re:run("A","\\x{41}") / re:run("a","(a)*+") / re:run("aaa","(a)*")
                                                        = {match,[{0,1}]} / {match,[{0,1},{0,1}]} / {match,[{0,3},{2,1}]}
re:run("abc","[[:alpha:]]+") / re:run("a+b","\\Qa+b\\E") = {match,[{0,3}]} / {match,[{0,3}]}
re:run("abc","^") / re:run("abc","") / re:run("","")     = {match,[{0,0}]} (all)
re:run("","",[notempty,{capture,all,index}])             = nomatch
re:run("abc","b",[notbol,{capture,all,index}])           = {match,[{1,1}]}
re:run("abc","a",[anchored,{capture,all,index}])         = {match,[{0,1}]} ; re:run("bac","a",[anchored,{capture,all,index}]) = nomatch
re:run("abc","a",[report_errors])                        = {match,[{0,1}]}
re:run("abc","a",[{match_limit,0},report_errors])        = {error,match_limit}  ; without report_errors = nomatch
re:compile("a(b")                                        = {error,{"missing closing parenthesis",3}}
re:compile("[a")                                         = {error,{"missing terminating ] for character class",2}}
re:compile("a{2,1}")                                     = {error,{"numbers out of order in {} quantifier",5}}
re:compile("(?<n>a)(?<n>b)")                             = error (duplicate subpattern name) ; with [dupnames] = ok
re:inspect(re:compile("(?<A>A)|(?<B>B)|(?<C>C)"), namelist) = {namelist,[<<"A">>,<<"B">>,<<"C">>]}
re:run("abc",123) / re:compile(123)                      = badarg
```
Also: `re:internal_run("xabcy",MP,[{offset,0}],true)` = `{match,[{2,2},{2,1},{3,1}]}` for `MP = re:compile("(b)(c)")` (same default all/index shape as `run`, non-global flat); with `global` → `{match,[[{2,2},{2,1},{3,1}]]}`; nomatch → `nomatch`.
The oracle's `re:version()` is `<<"10.47 2025-10-21">>`.

## Constraints
- `#![deny(unsafe_code)]` at the crate root — **no `unsafe`** here.
- Errors are `beam_common::{Result, BeamError}` only (`crate::error::badarg()` / `unsupported_bif()`); no crate-local error type, no `panic!`/`unwrap()` on runtime data.
- Never a wrong result: an unsupported pattern construct, option or subject outside the implemented subset must raise the typed `unsupported_bif` — never a silently wrong match and never a fabricated `{Msg,Pos}`.
- Term construction: `BifContext` has `int`/`atom`/`intern_atom`/`tuple`/`cons`/`list`/`make_float`/`boolean`/`ok` but **no** binary/map constructor — build binaries through `beam_codec::term::make_binary`/`make_bitstring` (see `../runtime/other.rs`'s siblings and `../binconv.rs`). Subject/pattern bytes and term inspection go through the crate-private `crate::access` / `crate::bits` and `beam_codec::term::*`.
- Allocation stays on `beam_alloc::Heap` via `BifContext`; the engine's own working state is plain Rust (`Vec`/`String`) — it never allocates a term on the process heap by itself.

## Test Strategy
- In-module `#[cfg(test)]` unit tests per file (parser shapes, option parsing, matcher spans, payload round-trip, error positions) — no oracle needed.
- The crate-level `../../tests/re.rs` integration file pins the oracle goldens above: it drives `default_registry().call("re", …)` (the crate's public seam), so it must work without the oracle present, and may *additionally* shell out to `erl` for live cross-checks (the `tests/checksum.rs` / `tests/calendar.rs` pattern). That file lives outside this node (the crate root owns `tests/`).
- Command: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_bifs --no-fail-fast`.

## Known Issues / honest typed gaps
- The compiled `Payload` is **ours**, not PCRE's — `inspect/2` supports only `namelist`; the oracle's PCRE-internals items (`size`, `options`, `capturecount`, …) must answer the typed `unsupported_bif` rather than a fabricated value.
- `run/2,3`'s automatic unanchored scan, `bsr_*`/`ucp`/`never_utf`, and any PCRE construct outside the subset listed above are typed gaps until implemented.
- Do **not** try to reproduce the oracle's `#Ref<…>` pattern id, nor PCRE's error strings that were not observed live.

## Routing Table
- `./mod.rs` -> the 8 BIF entry points + `register` + `REAL_BIFS`
- `./options.rs` -> compile/run option and capture-spec parsing
- `./syntax.rs` -> pattern text -> AST + `{Msg,Pos}` compile errors
- `./prog.rs` -> AST -> program + the self-contained payload image
- `./exec.rs` -> the matcher
- `./render.rs` -> captures -> terms
- `../runtime/other.rs` -> the parent family module that registers these real BIFs (`REAL_BIFS`)
- `../../CONTEXT.md` -> the crate root
