# public_key — priv (runtime data & generators)

## Intent
Runtime data files and their generators for the `public_key` application, plus the build-output location for the `c_src` NIF.
The one runtime data file is the Diffie-Hellman moduli set used by the SSH GEX key-exchange group helper.

## API Surface
- `moduli` (~580 KB) — RFC 4419-style DH moduli (one line per candidate: `Time Type Tests Tries Size Generator Modulus`, modulus in hex), license BSD-2-Clause. It is the input of the build-time generation of `../src/pubkey_moduli.hrl`.
- `convert.escript` — escript (`main([InFile,OutFile])`) invoked by the `src/Makefile` rule `pubkey_moduli.hrl: ../priv/moduli` as `escript ../priv/convert.escript $< $@`. It parses `moduli`, groups the accepted moduli by size, and emits an Erlang header defining `?dh_default_groups` (plus the license comment); the result is `../src/pubkey_moduli.hrl` (tracked in git).
- `generate` — bash helper (uses `ssh-keygen -G`/`-T`) that generates a `moduli-<bits>` file for each size in `1024 1536 2048 3072 4096 6144 7168 8192`; the outputs are meant to be concatenated into `moduli` (a manual, offline regeneration step).
- `lib/` and `obj/` — build output directories for the `../c_src` Windows NIF (git-ignored; only exist after a `win32` build).

## Constraints
- `moduli` and the generated `../src/pubkey_moduli.hrl` must stay consistent: change `moduli` then rebuild (the Makefile regenerates the header), never hand-edit either the header or reformat `moduli`.
- Do not commit build output under `lib/`/`obj/`.

## Notes for Agents
- `?dh_default_groups` (in `../src/pubkey_moduli.hrl`) is consumed only by `../src/pubkey_ssh.erl` for `public_key:dh_gex_group/4` / `dh_gex_group_sizes/0`.
- Regenerating `moduli` itself is a slow, manual ssh-keygen process, not part of the normal build; the committed `moduli` is the canonical data.

## See Also
- `../src/pubkey_ssh.erl` — the only consumer of the generated `?dh_default_groups`.
- `../src/Makefile` — hosts the `pubkey_moduli.hrl` generation rule.
- `../c_src/` — the NIF whose output lands in `priv/lib|obj`.
