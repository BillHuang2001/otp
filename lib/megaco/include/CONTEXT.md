# megaco — Public Include Headers

## Intent
The megaco application's exported headers: the record/macro definitions that form the public data-structure API for the Megaco/H.248 protocol stack.
These are the only files copied to the release (`lib/megaco-<VSN>/include/`), so external users of the megaco API compile against them.
The exact exported set is `EXTERNAL_HRL_FILES` in `../src/app/modules.mk` (all 5 headers below); adding/removing a public header means editing that list.

## API Surface (grouped by header)
- `megaco.hrl` — hand-written ("common data structures and error codes"). Defines the app-internal records `megaco_receive_handle`, `megaco_conn_handle`, `megaco_incr_timer`, `megaco_term_id`, plus a large macro set: termination-id wildcards (`?megaco_all`, `?megaco_choose`, `?megaco_root_termination_id`), predefined context/request ids, the full command `ErrorCode` number table, service-change reason strings ("900".."920"), and the IANA MGC ports (`2944` text / `2945` binary).
- `megaco_message_v1.hrl` (59 records) / `megaco_message_v2.hrl` (73) / `megaco_message_v3.hrl` (76) — ASN.1-compiler-generated record defs (`-record('MegacoMessage', ...)`, quoted names) mirroring the abstract ASN.1 module `MEDIA-GATEWAY-CONTROL-v{1,2,3}`. Top-level shapes: `'MegacoMessage'`/`'Message'`, `'TransactionRequest'`/`'TransactionReply'`/`'TransactionPending'`/`'TransactionAck'`, `'ActionRequest'`/`'ActionReply'`, `'CommandRequest'`, `'TerminationID'`, `'MediaDescriptor'`/`'StreamDescriptor'`/`'LocalControlDescriptor'`, `'EventsDescriptor'`/`'Signal'`, `'ServiceChangeParm'`, etc. v2 adds the `IndAud*` inductive-audit records; v3 adds `'SegmentReply'`, `'TermListAuditResult'`, `'RegulatedEmbeddedDescriptor'`.
  Each of the three files also hand-defines the non-ASN.1 record `megaco_event_parameter` (an escape hatch carrying an already-encoded `value` for external/unknown packages), which the generated text-codec headers `../src/text/megaco_text_gen_v{1,2,3}.hrl` rely on.
- `megaco_sdp.hrl` — hand-written SDP (RFC 4566) session-description records, all prefixed `megaco_sdp_` (e.g. `megaco_sdp_v/o/s/i/u/e/p/c/b/t/r/z/k/a/m` and the `megaco_sdp_a_*` attribute variants such as `_rtpmap`, `_fmtp`). Wrapped in an include guard (`-ifndef(megaco_sdp_).`).

## Constraints
- `megaco_message_v*.hrl` are ASN.1-compiler output: regenerate from `../src/binary/MEDIA-GATEWAY-CONTROL-v{1,2,3}.asn`, never hand-edit the records (the `megaco_event_parameter` record is the only intentionally hand-added entry).
- These headers are the public record API of the app; changing a record shape here changes what every in-tree codec and every downstream user compiles against, and must stay in sync with the BER/PER transformers and the text encoders/parsers for the matching version.
- A module must include at most ONE `megaco_message_v*.hrl`: the version records share the same record names, and neither `megaco.hrl` nor the message headers carry include guards, so mixing versions in one module is a record-redefinition error.
- Header-level `-include_lib("megaco/include/...")` is the convention used throughout `src/`, `test/`.

## Design Decisions
- `megaco.hrl` records are deliberately encoding-independent: the engine/transport layers (`src/engine/`, `src/tcp/`, `src/udp/`) and the codec code (`src/binary/megaco_binary_term_id*.erl`, `megaco_binary_transformer_v*.erl`, `src/text/*`) all `-include` it to carry `megaco_term_id` and the handles across the internal API.
- The message version headers correspond version-for-version to the ASN.1 abstract modules `MEDIA-GATEWAY-CONTROL-v1/v2/v3`, but they are NOT one-to-one with the generated modules under `../src/binary/` (that dir builds six per-encoding modules — `megaco_{ber,per}_media_gateway_control_v{1,2,3}` — as build artifacts, none of which are checked in as headers). The checked-in `megaco_message_v*.hrl` are the public, encoding-neutral copies of the `MEDIA-GATEWAY-CONTROL` record set.

## Known Issues
- `megaco.hrl` and all three message headers lack include guards (only `megaco_sdp.hrl` is guarded); guard against double inclusion at the consumer.
- v1's `'ServiceChangeProfile'` carries a commented-out single-field (`profileName`) variant and an active two-field (`profileName`, `version`) version — the single-field form is the "native" ASN.1 shape used when the encoder config selects `[native]`.

## Notes for Agents
- Do not enumerate/edit fields in the `megaco_message_v*.hrl` files by hand — treat them as generated and adjust `../src/binary/MEDIA-GATEWAY-CONTROL-v*.asn` instead.
- `megaco_sdp.hrl` has exactly two in-tree consumers: `../src/engine/megaco_sdp.erl` and `../test/megaco_sdp_SUITE.erl` (`test/` is a sibling directory — read-only from this node).
- Cross-reference `../src/binary/` (read-only sibling): it holds the `.asn` specs and the generated codec modules these headers mirror; escalate any needed change there to the parent node.

## Routing Table
| File | Description |
| --- | --- |
| `megaco.hrl` | Hand-written internal records (`megaco_term_id`, `megaco_receive_handle`, `megaco_conn_handle`, `megaco_incr_timer`) + wildcard/context-id/error-code/service-change-reason/port macros |
| `megaco_message_v1.hrl` | ASN.1-generated records for `MEDIA-GATEWAY-CONTROL-v1` (base H.248/Megaco v1) |
| `megaco_message_v2.hrl` | ASN.1-generated records for `MEDIA-GATEWAY-CONTROL-v2` (adds `IndAud*` inductive-audit types) |
| `megaco_message_v3.hrl` | ASN.1-generated records for `MEDIA-GATEWAY-CONTROL-v3` (adds `SegmentReply`, `TermListAuditResult`, `RegulatedEmbeddedDescriptor`) |
| `megaco_sdp.hrl` | Hand-written SDP (RFC 4566) records (`megaco_sdp_*`), guarded include |

## See Also
- `../src/binary/` — ASN.1 specs (`MEDIA-GATEWAY-CONTROL-v*.asn`) and per-encoding codec modules these headers mirror (read-only sibling).
- `../src/text/` — text encoders/parsers that include the matching `megaco_message_v*.hrl` and `megaco.hrl`.
- `../src/engine/` — engine, `megaco_sdp.erl` (only consumer of `megaco_sdp.hrl`), `megaco_user.erl`.
- `../CONTEXT.md` — megaco application overview.
