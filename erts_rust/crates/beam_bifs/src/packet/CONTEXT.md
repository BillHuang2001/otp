# `erlang:decode_packet/3` — packet families (`src/packet/`)

## Intent
The `decode_packet/3` BIF split along its packet families: `mod.rs` owns the BIF
itself — argument validation, the type switch, the dispatch and the result
rendering — and each family module owns the parser rules of one group of packet
types. The split exists so a family can be extended without touching another
family's file, and so a rule a family cannot express stays a local typed gap.

Ground truth (read-only): `../../../erts/emulator/beam/erl_bif_port.c:1476-1617`
(`decode_packet_3`) and `../../../erts/emulator/beam/packet_parser.c` /
`packet_parser.h` (`enum PacketParseType` 34-50, `packet_get_length` 248-501,
`packet_get_body` 152-164).

## API Surface — the FROZEN family seam (in-crate only; `mod.rs` is private)
`mod.rs` defines, and every family module codes against:
- `pub(super) enum PacketType` — one variant per `PacketParseType` member
  (`Raw`, `Len1`, `Len2`, `Len4`, `Line`, `Asn1`, `Sunrm`, `Cdr`, `Fcgi`, `Tpkt`,
  `SslTls`, `Http`, `HttpH`, `HttpBin`, `HttpHBin`).
- `pub(super) struct Options { pub max_plen: u64, pub trunc_len: u64, pub delimiter: u8 }`
  — the parsed `packet_size` / `line_length` / `line_delimiter` (`0` = "no limit").
- `pub(super) enum Body { Binary { start: usize, end: usize }, Term(Eterm) }` —
  the packet body: a slice of the input bytes, or a term the family already built
  (http family, `ssl_tls`). `Body::Binary` requires
  `start <= end <= packet_sz <= bytes.len()`.
- `pub(super) enum Outcome { Invalid, More(Option<usize>), Ok { packet_sz: usize, body: Body } }`
  — `Invalid` => `{error, invalid}`, `More(None)` => `{more, undefined}`,
  `More(Some(n))` => `{more, n}`, `Ok` => `{ok, Packet, Rest}` (or `{more, …}`
  when `packet_sz` is `0` or beyond the input).
- one entry point per family module:
  `pub(super) fn parse(ctx: &mut BifContext<'_>, ptype: PacketType, bytes: &[u8], opts: &Options) -> Result<Outcome>`
  returning `Err(crate::error::unsupported_bif())` for a type it does not
  implement.

`mod.rs` also owns the shared helpers `error_tuple`/`more_tuple`/`render`,
`resolve_type`, `parse_options`, `uint_value`, `atom_name`, `DEFAULT_DELIMITER`
and `register` (the `bif.tab:503` registration).

## Current implemented subset
**All fifteen `PacketParseType` members are implemented**; no valid packet type
is a typed gap.
- `framing.rs` — the length-extracting types: `raw`/`0`, the `1`/`2`/`4` integer
  length prefixes, `line`, and the five protocol headers `asn1`, `sunrm`, `cdr`,
  `fcgi`, `tpkt`. The `1`/`2`/`4`/`sunrm`/`cdr`/`fcgi`/`tpkt` arms share the C's
  `remain:` label (`remain`/`remain_body`), reproducing its 32-bit `hlen + plen`
  narrowing to the returned `int` — so an over-large `packet_size` makes both the
  wraparound and the signed reinterpretation observable (`{more, undefined}` for
  a wrapped `0`, `{error, invalid}` for a negative sum). `raw` and `line` extract
  their length directly and do not go through `remain`.
- `tls.rs` — `ssl_tls`: the normal record header and the SSLv2 ClientHello form,
  both rendered as the `{ssl_tls, [], ContentType, {Major, Minor}, Bin}` term.
- `http.rs` — `http`/`httph`/`http_bin`/`httph_bin`: both length rules, the request
  and status lines, the header-field/eoh/error forms and every URI shape.
- The only remaining `unsupported_bif` in the whole family is unclosable C
  undefined behaviour: an `ssl_tls` SSLv2 ClientHello whose wrapped declared
  length leaves fewer than its 3 header bytes (reachable only through
  `{packet_size, N}` with `N >= 2^32 - 3`), where the C hands `sys_memcpy` a
  negative size and aborts the emulator. `framing`'s analogous degenerate case —
  the wrapped `hlen + plen` sum landing below the `1`/`2`/`4` header size — is
  reported as the typed `{error, invalid}` rather than building an inverted body.

## Constraints
- **Argument validation precedes dispatch** (`erl_bif_port.c:1513-1548`): a bad
  `Bin`/`Opts` is `badarg` even for a rule that is not implemented; a type that is
  not a packet type at all (`bogus`, `3`, `cd`) is `badarg` in `resolve_type`.
- A family never fabricates a result: a rule it cannot decide is the typed
  `unsupported_bif`, and a failure the seam cannot express is an `Err`.
- Adding a packet type implementation must not change the seam or `mod.rs`'s
  validation/rendering — implement it inside the family module that receives its
  `PacketType`. New tests go in a sibling `tests/packet_<family>.rs`.
- Licensing Apache-2.0; every `*.rs` here carries the OTP
  `// %CopyrightBegin% … // %CopyrightEnd%` header block byte-exactly.

## Test Strategy
`tests/packet.rs` (crate-level, public API only) pins `raw`/`1`/`2`/`4`/`line`,
the argument validation (including "validated before the type is dispatched") and
one live `erl` cross-check. Each other family is pinned by its own file:
`tests/packet_framing.rs` (`asn1`/`sunrm`/`cdr`/`fcgi`/`tpkt`, plus the `1`/`2`/`4`
`remain:` narrowing), `tests/packet_tls.rs` (`ssl_tls`) and `tests/packet_http.rs`
(the four http types). Each live cross-check skips loudly when the oracle is
absent.

## Routing Table
- `mod.rs` → `decode_packet_3`, the `PacketType`/`Options`/`Body`/`Outcome` seam,
  validation, dispatch and rendering
- `framing.rs` → `raw`/`0`/`1`/`2`/`4`/`line`/`asn1`/`sunrm`/`cdr`/`fcgi`/`tpkt`
  length extraction (`packet_get_length`, incl. the shared `remain:` label)
- `tls.rs` → `ssl_tls` (record header + SSLv2 ClientHello)
- `http.rs` → `http`/`httph`/`http_bin`/`httph_bin`
