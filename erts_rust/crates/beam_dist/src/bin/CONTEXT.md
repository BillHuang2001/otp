# `beam_dist/src/bin` — the loopback CLI (`dist_probe`)

## Intent
A **bin target**, not part of the library API: `dist_probe loopback <report>` stands up two in-process endpoints (`alpha@localhost` acceptor / `beta@localhost` connector) over a real loopback socket, completes the `'N'` handshake both ways, and prints **exactly one** deterministic, host-independent stdout line (exit `0`).
It exists so the differential harness can drive contract A.5 as an external command, and so a byte-level wire claim can be re-checked without a Python/Erlang harness.
Self-contained over the crate's public API + `beam_codec`/`beam_term`/`beam_alloc`; safe Rust only (`#![deny(unsafe_code)]` + a `//!` doc, since crate lints are not inherited by a bin).

## API Surface (the frozen report contract)
| `<report>` | stdout (one line) |
|---|---|
| `ping` / `nodes` / `term` | `pong` / `1` (peers A handshook with) / `836803610161026103` = `hex(VERSION_MAGIC ++ body)` of the delivered `{1,2,3}` |
| `nodeid` | `nodeid flags=<hex> peer=beta@localhost creation=2 visibility=hidden` |
| `handshake` | `handshake flags=<hex> client=<hex> server=<hex>` — raw 2-byte-framed wire bytes of both directions, pinned challenge nonces |
| `badcookie` | `badcookie server=badarg client=badarg sent=Nr received=sN` |
| `frag` | `frag bytes=200000 sum=DC029F2D frames=4` (length + FNV-1a checksum + real `'E'`/`'F'` frame count) |
| `alias` | `alias from=1001 to=3003 msg=42` |
| `monitor` | `monitor watcher=1001 watched=5005 ref=9001 link from=1001 to=5006 exit from=1001 to=5007 reason=3003` |
| `regsend_missing` | `regsend target=missing outcome=nosuchprocess` |
| `utf8` | `837702CEA9` (uppercase hex ETF image of the non-Latin-1 atom that crossed the wire) |

- `ping`/`nodes`/`term` drive the mutual `{1,2,3}` round trip (A → B name-addressed `DOP_REG_SEND`, B echoes to A's pid as `DOP_SEND(_SENDER)`); their stdout is **frozen** — the harness's `dist/*` cases depend on those bytes.
- `regsend_missing` models the **receiver** with an `InboundSink`: ground truth (`dist.c:2484-2509`) is that a `DOP_REG_SEND` to an unregistered name is silently dropped, so no public-API path yields a `badarg` there.
- A missing/unknown subcommand ⇒ usage on **stderr**, exit `2`; a report that cannot be produced ⇒ `dist_probe: unsupported: <reason>` on stderr, exit `3` — never a fabricated line.

## Constraints
- Exactly one stdout line per report; no timing, no paths, no host names in the output.
- Only safe Rust; errors are `BeamError`, never a panic on wire data.

## Test Strategy
`tests/dist_probe_cli.rs` pins every literal (exact stdout, exit `0`, empty stderr) and byte-identity across repeated runs.

## Routing Table
- `dist_probe.rs` → subcommand parsing, the two-endpoint loopback harness, and the 11 report producers (this file is the authority; the table above is the contract).
