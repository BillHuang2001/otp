# erts/emulator/asmjit/ — Vendored AsmJit (JIT assembler backend)

## Intent
Verbatim copy of the third-party AsmJit C++ library (https://github.com/asmjit/asmjit): the low-level machine-code emitter/assembler that the BeamAsm JIT (`../beam/jit/`) generates x86-64 / AArch64 code with. This is a vendored dependency, **not** Erlang/OTP code.

## Status
- Third-party, Zlib license (`LICENSE.md`, `Copyright (c) 2008-2025 Petr Kobalicek`). Do not edit, reformat, or patch by hand — local changes are destroyed by the next upstream update.
- Pinned in `../asmjit.version` = `5fe1940275d04432da841896bac0a66cc2375551` (AsmJit v1.18+), and recorded in `vendor.info` (`ID: erts-asmjit`, `purl: pkg:github/asmjit/asmjit`, `annotation: "Vendor package modified in Erlang/OTP"`).
- `vendor.info` is an OTP-only file (SPDX/SBOM data, `exclude`d from the license scan) with an Ericsson Apache-2.0 header; it carries no upstream content.
- The "modified" annotation is a compliance classification by the OTP team. No OTP-specific `#ifdef`s, markers or source patches were found in the vendored sources/headers; the tree is upstream `src/asmjit` plus its `*.md` files.
- Only built when the emulator is configured/built with `FLAVOR=jit` (see `../CONTEXT.md`).

## Layout / API Surface
- Top-level umbrella headers: `asmjit.h`, `core.h`, `host.h`, `arm.h`, `a64.h`, `x86.h`, `ujit.h`, plus `asmjit-scope-begin.h` / `asmjit-scope-end.h` (MSVC `min`/`max` macro save/restore, included by the umbrella headers).
- `core/` (79 files — arch-independent API: `CodeHolder`, `Assembler`, emitters, formatters, `JitAllocator`, `cpuinfo`), `support/` (arena allocators, `Span`, bit helpers), `arm/` (AArch64), `x86/` (x86/x86-64), `ujit/` (Unified JIT), `README.md`, `CONTRIBUTING.md`, `LICENSE.md`, `asmjit.natvis`.
- Consumers include it as `#include <asmjit/core.h>`, so the *parent* directory must be on the include path (`-I$(TTF_DIR)` in-tree, `-I.` on upstream layouts).

## What Is Actually Compiled vs. Unused
- `../Makefile.in`: `ASMJIT_CPP = asmjit/core/*.cpp asmjit/support/*.cpp asmjit/$(JIT_ARCH)/*.cpp` (31 + 7 + 10 files); `JIT_ARCH` is `x86` or `arm`, chosen by `../configure.ac`.
- Only the selected architecture's `.cpp` are compiled, but the *headers* of **both** `arm/` and `x86/` are copied for the PCH because `core` sometimes references them.
- `ujit/` is unused here: it is C++17-only and excluded both from `ASMJIT_CPP` and from the header wildcard `ASMJIT_H` (i.e. not even copied to the build dir), and the build defines `-DASMJIT_NO_UJIT=1`.
- `ASMJIT_FLAGS` disables large parts of the library: `-DASMJIT_EMBED -DASMJIT_NO_BUILDER -DASMJIT_NO_COMPILER -DASMJIT_NO_UJIT -DASMJIT_NO_DEPRECATED -DASMJIT_STATIC -DASMJIT_NO_FOREIGN`. Whole translation units (e.g. `core/builder.cpp`, `core/compiler.cpp`, `x86/x86rapass.cpp`) are thereby compiled into empty objects (bodies are `#ifndef ASMJIT_NO_BUILDER`-style guarded) — the Builder/Compiler (register allocator) API is intentionally unavailable to the JIT.
- All headers are copied into `$(TTF_DIR)/asmjit/` and precompiled into `$(TTF_DIR)/asmjit/core.h.gch`, which is force-included (`-include`) into every emulator C++ file. That is why the copy step exists: cross-compilation needs a hermetic header tree.

## How to Update
`make update_asmjit` in `erts/emulator` (`../Makefile.in`): clones `git@github.com:asmjit/asmjit -b master` into `asmjit-copy`, writes the new SHA to `../asmjit.version`, `rm -rf asmjit`, copies `asmjit-copy/src/asmjit` → `asmjit/` and `asmjit-copy/*.md`, then `git add asmjit asmjit.version`. Requirements/gotchas: needs SSH access to GitHub, wipes this tree (re-check `vendor.info` fields and the `.github/scripts/otp-compliance.es` annotation afterwards), and upstream layout changes usually require matching `../Makefile.in` edits (v1.18 added `support/` and the ARM+x86 header copy). Never run it as a side effect of unrelated work.

## Known Issues
- `ASMJIT_CXXFLAGS` filters out `-Wformat`/`-Wformat=2` (and adds `@NO_MAYBE_UNINITIALIZED@`); upstream code is not warning-clean under OTP's flags by design.
- Not covered by the emulator's `format` / `format-check` clang-format targets — those glob only `beam/jit/**`, so `asmjit/` is implicitly excluded. Running clang-format on it produces a huge, pointless diff.
- Build artifacts from these sources land in `../$(TARGET)/<TYPE>/<FLAVOR>/asmjit/`, never here.

## Dependencies
- A C++ compiler (C++17-capable) plus PCH support; only relevant with `FLAVOR=jit`. Upstream build systems (CMake, etc.) are not used — OTP compiles the sources directly from `../Makefile.in`.

## See Also
- `../beam/jit/` → the BeamAsm JIT that consumes this library (only in-tree consumer)
- `../Makefile.in` → `ASMJIT_CPP`/`ASMJIT_H`/`ASMJIT_FLAGS`, PCH and `update_asmjit` rules
- `../asmjit.version`, `vendor.info`, `../CONTEXT.md` → pinning, SBOM metadata, emulator build knobs (`FLAVOR`/`TYPE`/`JIT_ARCH`)
