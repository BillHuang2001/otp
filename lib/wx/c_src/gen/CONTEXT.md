# wx C/C++ NIF glue — generated sources (`c_src/gen`)

## Intent
Output directory of the wx code generator (`../api_gen/`, `TARGET_CDIR = ../c_src/gen`).
Holds the machine-written C++ that bridges the Erlang `wx` API to wxWidgets C++ objects and to OpenGL/GLU.
The generator parses doxygen-produced XML of the wxWidgets and OpenGL headers (`../api_gen/wx_xml`, `../api_gen/gl_xml`) plus the `wxapi.conf`/`glapi.conf` specs and emits these files.
All files here are checked into git and MUST NOT be hand-edited.

## API Surface (what each generated unit provides)
wxWidgets side (compiled into the `wxe_driver` shared library):
- `wxe_wrapper_1.cpp` .. `wxe_wrapper_8.cpp` — the per-call wrappers `void <class>_<method>_<arity>(WxeApp*, wxeMemEnv*, wxeCommand&)`, split by a grouping counter so no single translation unit grows unbounded; each wrapper unmarshals `wxeCommand` args, calls the wx method and returns via `wxe_return`.
- `wxe_func_table.cpp` — defines `wxe_fns_t wxe_fns[]`, the dispatch table mapping every Erlang-visible `{class, method, arity}` to its wrapper function; entries are guarded by `#if WXE_WEBVIEW` etc. The `wxe_fns_t` type itself lives in the non-generated `../wxe_impl.h`.
- `wxe_events.cpp` — event machinery: `initEventTable()` (populates `wxeETmap etmap`, the event-type hash from atom ↔ wx event class), `wxeEventTypeFromAtom()`, event-object allocation and callback dispatch back to Erlang.
- `wxe_init.cpp` — `WxeApp::init_consts(wxeMetaCommand&)`: registers the global `wx` constants (e.g. `wxBLACK`, flags) into the `wx` module's constant table.
- `wxe_derived_dest.h` — header defining the `Ewx<Class>` derived wx classes whose destructors call `WxeApp::clearPtr(this)` (so C++-side destruction notifies the Erlang side), plus `WxeApp` helpers such as `delete_object`; included by the wrapper units.
- `wxe_macros.h` — extra `#include`s of wx headers needed by the generated code (all the `wx/...` headers, webview/EGL conditionals) and a few macro fixups; this is the `GENERATED_H` that every generated object depends on.

OpenGL side (compiled into the separate `erl_gl` shared library):
- `gl_nif.cpp` — GL NIF callbacks `void ecb_<glFunc>(ErlNifEnv*, ErlNifPid*, ERL_NIF_TERM argv[])`, one per GL entry point, plus `ecb_init_opengl`.
- `glu_nif.cpp` — same pattern for the GLU entry points.
- `gl_fdefs.h` — prototypes/`WXE_EXTERN` declarations for the GL function pointers (guarded by `WX_DEF_EXTS` for the defining vs. reference side).
- `gl_finit.h` — defines `gl_fns_t gl_fns[]`, the table binding each GL function id/name to its function pointer and `ecb_*` callback, terminated by a `{-1, NULL, NULL, NULL, NULL}` sentinel.

## Constraints
- Never edit any file in this directory by hand — edit `../api_gen/wxapi.conf` / `glapi.conf` (or the generator itself) and regenerate; source-controlled content only stays correct if it matches the generator.
- Regeneration requires the doxygen toolchain plus the OTP wxWidgets clone (`wx_api_gen` branch) and OpenGL headers; see `../api_gen/README` (generator aliases set `WX_DIR`/`GL_DIR`).
- Most files start with the `/***** This file is generated do not edit ****/` banner, but the marker is not uniform: `wxe_init.cpp` instead says `/* This file is also generated */`, and `wxe_func_table.cpp` / `wxe_macros.h` carry no such marker — the absence of the marker does NOT mean the file is hand-written.
- Generated code depends on hand-written headers (`../wxe_impl.h`, `../wxe_return.h`, `../wxe_helpers.h`, `../wxe_gl.h`) — those interfaces, not the generated bodies, are the contract to change.
- Files are compiled with optimization disabled (`CXX_O_NO_OPT`, rule `$(SYS_TYPE)/%.o: gen/%.cpp` in `../Makefile.in`); objects land in the platform `$(SYS_TYPE)/` dir.

## Notes for Agents
- ~100k lines total across 17 files; `wxe_init.cpp`, `wxe_func_table.cpp`, `gl_nif.cpp` and the wrappers are the largest. Do NOT open whole files or try to split/refactor them — grep for the symbol or class you need.
- To find where an Erlang `wx` call is implemented, grep the table in `wxe_func_table.cpp` for the `{class, method, arity}` entry, then grep the wrapper symbol in `wxe_wrapper_*.cpp`.
- Build wiring (`../Makefile.in`): `GENERATED_F` (the eight `wxe_wrapper_*`, `wxe_events`, `wxe_init`, `wxe_func_table`) + `GENERATED_H=gen/wxe_macros.h` feed `wxe_driver`; `gl_nif`/`glu_nif` (with `gl_finit.h`/`gl_fdefs.h`) feed `erl_gl`. `make complete_clean` in `../` removes generated sources to force regeneration.
- Do not expect the generator's Makefile target names (`wxe_funcs.cpp`, `gl_nif.c`) to match the checked-in filenames — trust the files here, not `../api_gen/Makefile`'s `C_TARGETS`/`GL_C_TARGETS` variables.

## See Also
- `../` (`lib/wx/c_src/`) — the hand-written NIF glue that includes and drives these generated units; its CONTEXT.md covers the overall driver layout.
- `../api_gen/` — the code generator (Erlang) that produces this directory; change specs there, not here.
- `../CONTEXT.md` — the `wx` application overview.
