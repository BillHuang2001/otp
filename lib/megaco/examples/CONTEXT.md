# megaco — Examples

## Intent
Runnable, non-runtime *examples* of the Megaco/H.248 application: a minimal MG/MGC pair (`simple/`) and the codec measurement/benchmark tooling (`meas/`).
Both are built and released as part of the megaco app (installed under `$(RELEASE_PATH)/lib/megaco-$(VSN)/doc/examples/…` and `$(RELEASE_PATH)/megaco_test/examples/…`).
This directory is a pure container — it holds no files of its own, only the two child directories.

## API Surface
- `simple/` — minimal Media Gateway (`megaco_simple_mg`) and Media Gateway Controller (`megaco_simple_mgc`), both `-behaviour(megaco_user)`; runnable smoke-check of the stack over TCP/UDP with text/binary codecs.
- `meas/` — codec measurement/benchmark modules (`megaco_codec_meas`, `megaco_codec_mstone1`, `megaco_codec_mstone2`, shared `megaco_codec_mstone_lib`, and the message-transform helper `megaco_codec_transform`).

## Constraints
- Both children are built via `lib/megaco/subdirs.mk` (`SUB_DIRS = src examples/simple examples/meas`), included from the app Makefile.
- `meas/Makefile` and `meas/*.sh.skel` are generated (autoconf from `Makefile.in`, templating from `*.sh.skel.src`) and are gitignored — never edit the generated files.
- New modules in either child must be listed in that child's local `modules.mk`.

## Notes for Agents
- These are examples, not megaco runtime code: they depend on the megaco `ebin` and public APIs but the megaco app does not depend on them.

## Test Strategy
- Exercised by the sibling suite `../test/megaco_examples_SUITE.erl` (`simple/1`, `meas/*` — read-only from here; escalate changes to the parent).

## See Also
- `../CONTEXT.md` — megaco application root; `../test/megaco_examples_SUITE.erl` — tests for these examples (read-only sibling).
- `../doc/guides/megaco_examples.md` — published examples guide.

## Routing Table
- `simple/` → runnable minimal MG (`megaco_simple_mg`) + MGC (`megaco_simple_mgc`) example, with its own Makefile.
- `meas/` → codec measurement/benchmark tools (`meas`, `mstone1`, `mstone2`) + message-pack transform utility, with autoconf-generated Makefile and run scripts.
- `../test/megaco_examples_SUITE.erl` → test suite covering the examples (sibling — read-only, escalate writes to parent).
