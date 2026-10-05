# megaco — Binary (ASN.1 BER / PER) codec

## Intent
Implements the two *binary* codecs of the Megaco/H.248 app on top of the `asn1` application:
ASN.1 BER (the H.248 Annex A / RFC 3016 "binary encoding of the protocol", the standard/recommended codec)
and ASN.1 PER (a non-standard, non-interoperable alternative that is shipped as-is).
Each codec exists in three protocol versions (v1/v2/v3), plus the hand-written layer that converts between the
Megaco internal Erlang record form and the ASN.1 codec form.
All ASN.1 code generation for the app happens *in this directory* at build time.

## API Surface
Codec entry points (all `-moduledoc false`; users reference them by name in `{encoding_mod, Mod}` /
`encoding_config`, but they are internal modules — the documented API is `megaco:encode_message/...` etc.):

- `megaco_binary_encoder` — the recommended BER codec (historically "ber_bin").
  Implements the `megaco_encoder` behaviour and is the only binary codec with true dynamic version detection
  (`decode_message(EC, dynamic, Bin)` decodes the version via a partial decode, then dispatches) and partial
  (`decode_mini_message`) decoding.
  Despite its generic name it is **BER-only** — it never dispatches to PER; it is the BER variant built from
  `.asn1config`-enabled generated modules (`megaco_ber_media_gateway_control_vN` + `megaco_binary_transformer_vN`).
- `megaco_ber_encoder` — the older BER codec; same generated BER modules, but `decode_message(EC, dynamic, Bin)`
  reverts to version 1 and the transaction/action encoders are stubs returning `{error, not_implemented}`.
- `megaco_per_encoder` — PER codec (`megaco_per_media_gateway_control_vN` + the same transformers).
  `decode_message(dynamic, _)` reverts to version 1 and `decode_mini_message/3` returns `{error, not_implemented}`
  (PER cannot partially decode).
- Common exports of the three codecs: `encode_message/2,3`, `decode_message/2,3`, `decode_mini_message/3`,
  `encode_transaction/3`, `encode_action_requests/3`, `encode_action_request/3`, `encode_action_reply/3`,
  plus `version_of/2` (not a behaviour callback).

Internal support modules (never called by users directly):

- `megaco_binary_encoder_lib` — codec-agnostic driver shared by all three codecs: dispatches to a
  `AsnMod`/`TransMod` pair, implements `decode_message_dynamic/4` and `decode_mini_message_dynamic/4`,
  handles the `[native]` encoding config (skip the transformation) and the internal `binary`/`io_list` forms.
- `megaco_binary_transformer_vN` — exports `tr_message/3`, `tr_transaction/3`.
  Converts between the internal Megaco records (`include/megaco_message_vN.hrl`) and the ASN.1 codec form, and
  validates constraints; the `Mode` argument is `encode` | `decode` (| `verify`), and the config selects the
  resolver (`[native]`, `[verify]`, `[]` → default resolver + `[8,8,8]` levels, `[{binary_name_resolver,{Mod,Opts}}]`).
- `megaco_binary_name_resolver_vN` — exports `packages/0`, `capabilities/0,1`, `encode_name/3`, `decode_name/3`.
  Maps ASN.1 string names ↔ atoms for mid/package/property/dialplan/etc. using the per-version Megaco package
  registries embedded in the module (hence the large file sizes); delegates termination-id handling to
  `megaco:encode_binary_term_id/2` / `megaco:decode_binary_term_id/2`.
- `megaco_binary_term_id` — exports `encode/2`, `decode/2`; turns a `#megaco_term_id{}` into/from an ASN.1
  `#'TerminationID'{}` (handles the ROOT/ALL/CHOOSE special cases and `[8,8,8]` default level config).
- `megaco_binary_term_id_gen` — the bit-level codec for termination ids: `encode_without_wildcards/2`,
  `encode_with_wildcards/2`, `decode_without_wildcards/2`, `decode_with_wildcards/3`.

Build / ASN.1 inputs (tracked):

- `MEDIA-GATEWAY-CONTROL-v{1,2,3}.asn` — the H.248 Annex A ASN.1 specifications (ASN.1 module names
  `MEDIA-GATEWAY-CONTROL-vN`; v2/v3 carry an object identifier).
- `megaco_{ber,per}_media_gateway_control_v{1,2,3}.set.asn` — one-line ASN.1 "set" files, each listing exactly one
  `.asn` file; the *set file name* determines the generated Erlang module name, which is how one `.asn` yields
  distinct BER and PER modules.
- `megaco_ber_media_gateway_control_v{1,2,3}.asn1config` — BER-only exclusive-decode instructions, all naming the
  module itself: `decode_message_trans_partial`, `decode_message_acts_partial`, `decode_message_version`,
  `decode_message_mId`.
- `modules.mk` — `MODULES` (hand-written + generated module list), empty `INTERNAL_HRL_FILES`,
  `ASN1_VN_SPEC` / `BER_ASN1_VN_SPEC` / `PER_ASN1_VN_SPEC`.
- `Makefile` — targets `opt` (default), `prebuild`/`prebuild.skip`, `v1`/`v2`/`v3`/`specs`, `clean`, `info`,
  `release_spec` (installs `$(ERL_FILES) $(HRL_FILES) $(ASN1_FILES)` into `$(RELEASE_PATH)/lib/megaco-$(VSN)/src/binary`).
- `depend.mk` — the generation rules (`erlc -bber` / `-bper`) and per-module header dependencies.

Generated at build time into this directory (git-ignored, never hand-edited):
`megaco_{ber,per}_media_gateway_control_v{1,2,3}.erl`, `.hrl` and `.asn1db`.

## Constraints
- Never edit the generated `megaco_*_media_gateway_control_*.erl`, `.hrl` or `.asn1db` files; change
  `MEDIA-GATEWAY-CONTROL-vN.asn`, the `.set.asn` or (for BER) the `.asn1config`, then rebuild.
- The `asn1` application (its `erlc` back ends) must be available to build this directory; the generated modules
  additionally need the ASN.1 runtime at run time unless built with `+inline`.
- A new/renamed module here must be added to `modules.mk` *and* to `../app/megaco.app.src` (the app file lists the
  binary modules explicitly, including the generated ones).
- Keep the `.asn` specs and the checked-in `include/megaco_message_v{1,2,3}.hrl` (and
  `../engine/megaco_message_internal.hrl`) in sync by hand: the transformers are compiled against the checked-in
  records while the generated modules use their own `.hrl`, and they must describe identical record shapes.
- The v1/v2/v3 (and BER/PER) duplication is deliberate — all three protocol versions are shipped and must stay
  separately addressable; do not factor them into one shared module.
- `ASN1_CT_OPTS` must keep `+legacy_erlang_types`: without it the generated records use the post-R16
  `OCTET STRING`/`BIT STRING` representations and no longer match the checked-in megaco records.

## Design Decisions
- BER and PER are produced by separate `erlc -bber` / `-bper` runs over separate `.set.asn` wrapper files rather
  than by one shared module: the back ends generate different encode/decode APIs and representations, and the
  module names must differ.
- `+noobj` — let the ASN.1 compiler generate only `.erl`/`.hrl`; the OTP build then compiles the generated Erlang
  itself so it can apply its own flags (`-Ddebug`, `+deterministic`, warning options via `MEGACO_ERL_COMPILE_FLAGS`).
- `+legacy_erlang_types` — keep the pre-R16 `OCTET STRING`/`BIT STRING` representation so the generated records
  stay byte-compatible with the checked-in megaco records that the transformers use.
- `+asn1config` on BER only — the exclusive decode gives the fast partial decoders (`decode_message_version/1`,
  `decode_message_mId/1`) that enable `dynamic` version detection and mini decode; PER has no equivalent, so PER
  neither detects versions dynamically nor mini-decodes.
- `+deterministic` when `ERL_DETERMINISTIC=yes` — reproducible builds (strip absolute paths / compile options).
- `+inline` under `MEGACO_INLINE_ASN1_RT=true` — inline the ASN.1 runtime into the generated modules.
- The generated BER/PER modules and the two name-resolver/transformer generations are wired together only at run
  time (module names are passed as atoms/variables), so the codecs have no compile-time dependency on the
  generated code; `depend.mk` provides the make-level ordering instead.
- Backward-compatibility clauses: every codec API accepts a leading `{version3, V3}` element in the encoding
  config (`[{version3, ...}|EC]`) so both the old and the plain config forms work.

## Known Issues
- `megaco_binary_encoder` is BER-only despite its generic name; PER is reachable only via `megaco_per_encoder`.
  (The docs also mention a `megaco_ber_bin_encoder`, which no longer exists.)
- All three codecs return `{error, not_implemented}` for `encode_transaction/3`, `encode_action_requests/3`,
  `encode_action_request/3` and `encode_action_reply/3`; only whole-message encode/decode is implemented, so
  transaction/action-first encoding, `megaco:encode_actions/3` and segmentation do not work with the binary codecs.
- The documented binary `encoding_config` forms `[LevelConfig]` (e.g. `[3,8,5,8]`) and a bare `integer()` level
  count are not matched by `tr_message/3`/`tr_transaction/3` (which only accept `[native]`, `[verify]`, `[]` and
  `[{binary_name_resolver, {Mod, Opts}}]`); such a config reaches the transformer as a `case_clause` error,
  wrapped as `{error, ...}` by `megaco_binary_encoder_lib`. Non-default levels must be given through
  `{binary_name_resolver, {Mod, Levels}}`.
- `MEGACO_INLINE_ASN1_RT` (the `+inline` switch in `depend.mk`) is not set anywhere in the repository, so that
  branch is dead unless a build sets it manually.
- `depend.mk` records no header dependency for `megaco_binary_name_resolver_v3.erl`,
  `megaco_binary_term_id.erl` and `megaco_binary_term_id_gen.erl`, although they include
  `../app/megaco_internal.hrl` and `../engine/megaco_message_internal.hrl`; edits to those headers may not trigger
  a recompile of these modules.
- The `v1` target in the `Makefile` depends on `$(V2_SPEC_BINS)` (copy/paste slip); harmless only because `specs`
  builds v1/v2/v3 together.
- The exclusive-decode entries `decode_message_trans_partial` and `decode_message_acts_partial` are declared and
  generated but called nowhere in the current tree.
- `release_spec` ships the generated `.erl`/`.hrl` sources plus the `MEDIA-GATEWAY-CONTROL-vN.asn` specs, but not
  the `.set.asn` / `.asn1config` inputs, so the shipped sources cannot be regenerated as-is.

## Notes for Agents
- The generated `.erl`/`.hrl`/`.asn1db` files are absent from a fresh checkout (git-ignored) — build first
  (`make opt` here, or build `lib/megaco`) before reading or grepping the generated codecs.
- Encoding config semantics are spread across code: the config is passed verbatim from
  `../engine/megaco_messenger_misc.erl` to the codec, then to `megaco_binary_encoder_lib`, then to the transformer /
  name resolver and finally to `megaco_binary_term_id` (`[8,8,8]` default level config lives there).
- `include/megaco_message_v{1,2,3}.hrl` and `../engine/megaco_message_internal.hrl` are themselves
  ASN.1-compiler-generated records ("Generated by the Erlang ASN.1 compiler version:1.2.7") that are checked in;
  they are the contract the transformers are compiled against.
- Version numbers in the file names (`_v1/_v2/_v3`) refer to the Megaco protocol version, not to the OTP release.
- Everything here is version-specific: when changing a codec, the corresponding v1/v2/v3 file usually needs the
  same change (the transformers/resolvers are near-identical copies differing in record/package sets).

## Dependencies
- Build: the `asn1` application's `erlc` back ends (`-bber`, `-bper`, `+asn1config`, `+legacy_erlang_types`,
  `+noobj`, `+deterministic`, `+inline`) and make.
- Run time: the `asn1` runtime library (unless the generated code was inlined) and the `megaco` app itself.

## Test Strategy
- Tests live in `../test/`, not here: `megaco_codec_v{1,2,3}_SUITE` (per-version encode/decode round-trips through
  `megaco_binary_encoder`, `megaco_ber_encoder` and `megaco_per_encoder`, plus generated-message fixtures),
  `megaco_codec_mini_SUITE` (partial/`decode_mini_message` decoding) and `megaco_binary_term_id_SUITE`
  (termination-id encode/decode with and without wildcards).
- `../test/megaco_codec_test_lib.erl` is the shared driver for the codec suites; `../examples/meas/` contains the
  codec throughput benchmarks.
- The test suites are the practical way to verify a regenerated codec; a codec change normally requires building
  `lib/megaco` and running the v1/v2/v3 codec suites.

## See Also
- `../engine/megaco_encoder.erl` — the `megaco_encoder` behaviour implemented by the three codecs.
- `../engine/megaco_messenger.erl`, `../engine/megaco_messenger_misc.erl` — callers of
  `encode_message`/`decode_message`/`decode_mini_message` and owners of the encoding config.
- `../app/megaco.erl` — `encode_binary_term_id/2` / `decode_binary_term_id/2` (used by the name resolvers);
  `../app/megaco_internal.hrl` — debug macros included by the resolvers/transformers; `../app/megaco.app.src` —
  the app's module list.
- `../../include/megaco_message_v{1,2,3}.hrl`, `../../include/megaco.hrl` (`#megaco_term_id{}`) and
  `../engine/megaco_message_internal.hrl` — the records exchanged with the transformers.
- `../text/` — the text codecs, structurally analogous; `../flex/` — the optional linked-in text scanner.
- `../test/megaco_codec_*_SUITE.erl`, `../test/megaco_binary_term_id_SUITE.erl` — test suites (sibling, read-only).
