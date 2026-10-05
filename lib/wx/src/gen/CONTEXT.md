# wx — Generated Class Modules

## Intent
Machine-generated Erlang bindings for the wxWidgets class hierarchy (236 `wx*.erl` modules) plus the OpenGL bindings `gl.erl`/`glu.erl`.
Every module is derived from the C++ headers and the embedded wxWidgets/OpenGL documentation.

## API Surface
- `wx<class>.erl` — one module per wxWidgets class; each exports `new/N`, `destroy/1` where applicable, the class's member functions, and re-exports inherited functions; `parent_class/1` implements the inheritance chain used by `wx.erl`'s `?CLASS` macros.
- `gl.erl`, `glu.erl` — OpenGL / GLU function bindings (loaded via `wxe_master:init_opengl/0`).
- `wxe_funcs.hrl` — generated macro header of per-function driver opcodes; `-include`d from `../wxe.hrl`.
- `vendor.info` — SPDX metadata for the third-party wxWidgets/OpenGL documentation embedded in the generated sources (not compiled).

## Constraints
- DO NOT EDIT any file here. Every `.erl` starts with `%% This file is generated DO NOT EDIT`.
- Sources of truth are the wxWidgets documentation/headers processed by `../../api_gen/`; regenerate, never patch in place.
- The module list is picked up by `../Makefile` via `wildcard gen/wx*.erl` plus explicit `gen/gl.erl`/`gen/glu.erl`, so a new file is compiled automatically but must also be regenerated into `../wx.app.src`'s `%GEN_MODS%`.

## Notes for Agents
- These files are large and highly repetitive; read a single class (e.g. `wxFrame.erl`) to understand the pattern rather than scanning the whole directory.
- `wxe_funcs.hrl` is ~140 KB of macros — grep it, don't read it whole.

## See Also
- `../../api_gen/` — the generator (sibling, read-only).
- `../wxe.hrl` — includes `gen/wxe_funcs.hrl`.
- `../CONTEXT.md` — the handcrafted modules that consume these bindings.
