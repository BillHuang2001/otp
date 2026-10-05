# Megaco Text Codec (H.248 Annex B text encoding)

## Intent
Implements the Megaco/H.248 *text* encoding: decoding a text message into `'MegacoMessage'` record trees and encoding those trees back to text.
Two flavours exist as fully parallel module trees: *pretty* (readable, indented) and *compact* (minimal whitespace).
Everything is hand-written Erlang; there is no ASN.1 compiler involvement (unlike `../binary/`).
The codec is version-triplicated: modules, grammars and header tables exist per H.248 protocol version (1/2/3).

## API Surface
Public codec entry points — they implement the `megaco_encoder` behaviour and are selected by `{encoding_mod, Mod}` in the receive handle:
- `megaco_pretty_text_encoder.erl` — the *default* encoding module (`../engine/megaco_config.erl` sets `encoding_mod = megaco_pretty_text_encoder`); exports `encode_message/3`, `decode_message/3`, `decode_mini_message/3`, `encode_transaction/3`, `encode_action_requests/3`, `encode_action_request/3`, `encode_command_request/3`, `encode_action_reply/3`, dispatching each to the `_v1/_v2/_v3` module by protocol version.
- `megaco_compact_text_encoder.erl` — same behaviour/API, compact output.
- Both additionally export `version_of/2`, `token_tag2string/1,2` (re-exported by `../app/megaco.erl` as `megaco:token_tag2string/2,3`), `trim_quoted_string/1` + `term_to_{compact,pretty}_string/1` (pretty only), and legacy 2-arity/1-arity wrappers (`encode_message/2`, `decode_message/2`, `encode_transaction/1`, `encode_command_request/1`, `encode_action_reply/1`).
Version-specific encoders (callable, unit-tested, but not behaviour callbacks — treat as internal):
- `megaco_pretty_text_encoder_v{1,2,3}.erl`, `megaco_compact_text_encoder_v{1,2,3}.erl` — `encode_*` for one fixed version; each includes `../../include/megaco_message_vN.hrl` + `megaco_text_tokens.hrl` + the matching `megaco_text_gen_vN.hrl`; v1/v2 also define `encoder_pre_version_3` (used as an `-ifdef` switch in `megaco_text_tokens.hrl`).
Scanner / decoder:
- `megaco_text_scanner.erl` — pure-Erlang scanner, `scan/1` on a binary or char list; returns `{ok, Tokens, Version, LastLine}` (version read from the `!/N` or `megaco/N` token) or `{error, Reason, Line}`; rejects input over `?MAX_SCAN_SIZE` (100000 bytes); also exports `skip_sep_chars/2`, reused by `../engine/megaco_digit_map.erl`.
- `megaco_text_mini_decoder.erl` — `decode_message/2`; always scans with `megaco_text_scanner` (never the flex NIF), strips the token list at the first `Trans`/`Reply`/`Pending`/`ResponseAck`/`Error` token plus `endOfMessage`, and parses with `megaco_text_mini_parser`, returning a partly populated `'MegacoMessage'` (version + mId only) for `megaco_messenger`'s error path.
Parsers (Yecc; `.yrl` is the source of truth, the generated `.erl` is gitignored build output):
- `megaco_text_parser_v{1,2,3}.yrl` — `Rootsymbol megacoMessage`, full Annex B grammar, semantic actions in `megaco_text_parser_vN.hrl`.
- `megaco_text_mini_parser.yrl` — `Rootsymbol megacoMessage` but only covers `version mId` (plus optional authenticationHeader), i.e. the message header.
Tracked hand-maintained headers (not build products):
- `megaco_text_tokens.hrl` — all token atom/string macros, the `?property_parm` record, and the `?classify_char{,2,3,4}` character classifiers; the pivot file for the whole codec, also included by `../engine/megaco_digit_map.erl`.
- `megaco_text_parser_vN.hrl`, `megaco_text_mini_parser.hrl` — Erlang semantic-action code `-include`d from the `Erlang code.` section at the end of the corresponding `.yrl`; `.yrl` and `.hrl` are one logical unit.
- `megaco_text_gen_vN.hrl` — the `enc_*`/`dec_*` value-to-text tables `-include`d at the end of the `_vN` encoders (2404/2793/3463 lines).
Build glue: `Makefile` (`EBIN = ../../ebin`, `MEGACO_INCLUDEDIR = ../../include`, `parser`/`parser_vN` targets, `TYPE=debug` → `-Ddebug`), `modules.mk` (`MODULES`, `INTERNAL_HRL_FILES`, `INTERNAL_YRL_FILES`), `depend.mk` (per-beam dependencies).

## Constraints
- Never hand-edit `megaco_text_parser_v{1,2,3}.erl` / `megaco_text_mini_parser.erl`; they are gitignored build output regenerated from the `.yrl` by the `.yrl` → `.erl` rule in `$(ERL_TOP)/make/$(TARGET)/otp.mk` (generated from `make/otp.mk.in`, and included at the top of `Makefile`).
- Edit a `.yrl` and its paired `.hrl` together, and keep `Expect N` (expected shift/reduce conflicts: v1 609, v2 137, v3 94, mini 1) in sync — yecc errors if the real count differs.
- Keep the version triplication deliberate: do not merge or parameterize `_v1/_v2/_v3`; mirror a change across versions only where the H.248 version shares the construct.
- Adding a token requires touching `megaco_text_tokens.hrl`, the relevant `.yrl` (`safeToken`/`safeToken2` rules, `make_safe_token/1`), the paired `.hrl`, the affected `_vN`/pretty/compact encoders, `megaco_text_scanner.erl`, and the flex scanner token table in `../flex/megaco_flex_scanner_drv.flex.src`.
- The scanner, parsers and encoders share one token shape, `{TokenTag, Line, Text}` (the parsers wrap it via `safeToken` → `{safeToken, Line, Text}`); do not change it locally.
- `?MAX_SCAN_SIZE` (100000 bytes) and `?MAX_DIGITS` (20) are deliberate anti-DoS bounds; do not remove or raise them.
- `megaco_flex_scanner:scan/2` may only be called from the two dispatcher modules — never from the scanner/mini decoder.
- Files added or renamed here must be registered in both `modules.mk` and `depend.mk`.

## Design Decisions
- Encoding config selects the scanner, not a compile flag: `[]` → Erlang `megaco_text_scanner`; `[{flex, Port}]` → `megaco_flex_scanner:scan/2` (NIF, see `../flex`); `ENABLE_MEGACO_FLEX_SCANNER` is purely a `../flex` build concern and nothing in this directory is conditionally compiled on it.
- Protocol version is discovered from the token stream, so `decode_message/3` accepts `dynamic` (the normal case) and picks the `_vN` parser afterwards; `encode_message/3` reads `#'Message'.version` from the record.
- Legacy `{version3, _}` encoding-config entries are swallowed by `<BACKWARD-COMPAT-CLAUSE>` clauses that simply strip the option before delegating.
- `decode_mini_message/3` deliberately avoids the flex scanner and tolerates a full message by truncating the token stream, so it can still extract the remote mid from a message the full decoder rejected.
- Every version parser declares all token terminals, including later-version tokens (marked `OTP-7534`), so the shared scanner's output is always grammatical; unsupported constructs are rejected by the grammar, not by the scanner.
- Pretty and compact are separate module trees rather than a mode flag, so each can be read and tuned independently.

## Known Issues
- Duplication is structural: `megaco_pretty_text_encoder_v{1,2,3}.erl` and the compact counterparts each repeat ~200-350 lines of near-identical `encode_*` clauses, so a fix typically has to be applied 3-6 times; grep the function name across the directory before considering a change complete.
- Both dispatchers contain four almost identical `decode_message/3` clause blocks (one per encoding-config shape) with commented-out dead error arms; edits are easily applied to only one block.
- `decode_message/3` matches the encoding config *exactly* (`[]`, `[{flex, Port}]`, `[{version3,v3}]`, `[{version3,v3},{flex,Port}]`); any other shape or ordering yields `{error, {bad_encoding_config, EC}}`.
- `scan_error/…` in the dispatchers already handles the `{error, Reason, Tokens, Line}` 4-tuple that only `tokens1/3` ever produces (`no_version_found`); the `{error, Reason, Line}` 3-tuple is the common case.
- The token-addition checklist comment at the top of `megaco_text_tokens.hrl` is stale: it names `megaco_text_gen.hrl` and `megaco_text_parser.yrl` (no version suffix) and omits the mini parser and the flex scanner C token table.
- `megaco_pretty_text_encoder_v2` exports a stray `test/1` helper that is not part of the codec API.
- `megaco_text_gen_v*.hrl` are machine-shaped (originally derived from the H.248 ASN.1 with manual patches) and very large; do not reformat them.

## Notes for Agents
- `depend.mk` is the authoritative per-beam dependency list — read it before adding includes.
- The generated parser `.erl` files do not exist in a fresh checkout; run `make` (or `make parser`) here before anything that loads `megaco_text_parser_vN` / `megaco_text_mini_parser`.
- `MEGACO_PARSER_INLINE` (on unless set to `false`) adds `-Dmegaco_parser_inline`, enabling `-compile({inline, ...})` for the semantic actions in the `.hrl` files; `TYPE=debug` adds `-Ddebug`.
- The scanner threads a line number through every token triple and reports it as `LastLine`; that value is what reaches the user in `{error, [{reason, …}, {token, …}, {chars, …}]}`.
- Errors from decode are normalised by `scan_error/…` and `parse_error/…` in the encoders, which also rewrite `bad_property_parm` reasons.
- `checkin`/test helpers live in the sibling `../test/` directory, not here.

## Dependencies
- No external system dependencies; the flex scanner it can delegate to lives in `../flex/` and needs `flex` plus a C compiler to build (optional at configure time).

## Test Strategy
- Covered by the sibling suites `../test/megaco_codec_v1_SUITE.erl`, `_v2_SUITE`, `_v3_SUITE` (pretty and compact, with and without the flex scanner) and `../test/megaco_codec_mini_SUITE.erl`; shared helpers are in `../test/megaco_codec_test_lib.erl`, `../test/megaco_codec_flex_lib.erl`.
- Run from `lib/megaco/test` (`make test` / `ct_run`); the flex-vs-Erlang comparison cases only run when the flex driver was built (`ENABLE_MEGACO_FLEX_SCANNER=true`).

## See Also
- `../engine/megaco_encoder.erl` — the behaviour implemented by the two dispatcher modules.
- `../engine/megaco_messenger.erl` — calls `decode_message/3` and `decode_mini_message/3`.
- `../engine/megaco_config.erl` — default `encoding_mod`, `scanner` env and the `text_config` entry the encoding config is built from.
- `../engine/megaco_digit_map.erl` — includes `megaco_text_tokens.hrl` and calls `megaco_text_scanner:skip_sep_chars/2`.
- `../binary/` — the ASN.1 PER/binary codec, the parallel implementation of the same behaviour.
- `../flex/` — optional flex-based NIF scanner selected via `[{flex, Port}]`; duplicates the token table in C.
- `../../include/megaco_message_v{1,2,3}.hrl` — version-specific record shapes encoded by the `_vN` modules.
- `../test/` — text/mini codec test suites (sibling — read-only, escalate writes to `lib/megaco`).
