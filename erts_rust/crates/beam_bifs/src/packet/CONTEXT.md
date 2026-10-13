# `erlang:decode_packet/3` — packet families (`src/packet/`)

## Intent
The `decode_packet/3` BIF split along its packet families: `mod.rs` owns the BIF
itself — argument validation, the type switch, the dispatch and the result
rendering — and each family module owns the parser rules of one group of packet
types. The split exists so a family can be implemented (or extended) without
touching another family's file, and so an unimplemented type is a local typed gap.

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
- `framing.rs` — the `raw`/`0`, `1`, `2`, `4` and `line` length rules (moved
  verbatim from the pre-split `packet.rs`). These five are the only packet types
  with a real parser.
- `tls.rs` — `ssl_tls`: typed `unsupported_bif`.
- `http.rs` — `http`/`httph`/`http_bin`/`httph_bin`: typed `unsupported_bif`.

Every other valid type (`asn1`, `sunrm`, `cdr`, `fcgi`, `tpkt`) is dispatched to
`framing::parse` and is a typed `unsupported_bif` there.

## Constraints
- **Argument validation precedes dispatch** (`erl_bif_port.c:1513-1548`): a bad
  `Bin`/`Opts` is `badarg` even for an unimplemented type; a type that is not a
  packet type at all (`bogus`, `3`, `cd`) is `badarg` in `resolve_type`.
- A family never fabricates a result: an unimplemented rule is the typed
  `unsupported_bif`, and a failure the seam cannot express is an `Err`.
- Adding a packet type implementation must not change the seam or `mod.rs`'s
  validation/rendering — implement it inside the family module that receives its
  `PacketType`. New tests go in a sibling `tests/packet_<family>.rs`.
- Licensing Apache-2.0; every `*.rs` here carries the OTP
  `// %CopyrightBegin% … // %CopyrightEnd%` header block byte-exactly.

## Test Strategy
`tests/packet.rs` (crate-level, public API only) pins the five framing types, the
argument validation (including "validated before the type is dispatched") and one
live `erl` cross-check, which skips loudly when the oracle is absent. A family's
own behaviour is pinned by its own `tests/packet_<family>.rs`.

## Routing Table
- `mod.rs` → `decode_packet_3`, the `PacketType`/`Options`/`Body`/`Outcome` seam,
  validation, dispatch and rendering
- `framing.rs` → `raw`/`0`/`1`/`2`/`4`/`line` length extraction (`packet_get_length`)
- `tls.rs` → `ssl_tls` (typed gap at present)
- `http.rs` → `http`/`httph`/`http_bin`/`httph_bin` (typed gap at present)
