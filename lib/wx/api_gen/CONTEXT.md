# wx API Generator (`api_gen/`)

## Intent
Code-generation pipeline that produces the `wx` application's bindings to wxWidgets and OpenGL from the upstream C++ headers.
Two-stage: doxygen parses the C++ headers into XML, then Erlang generator modules read that XML plus a human-maintained spec file and emit the Erlang wrapper modules, the C++ NIF glue, and the public headers.
This is the build-adjacent, developer-only part of `lib/wx/`; end users of `wx` never run it.

## API Surface

### wx (wxWidgets) generator suite
- `wx_gen.erl` — driver/top-level; entry point `code/0` (`devcode/0` for interactive runs).
  Consult `wxapi.conf`, parse the doxygen XML in `wx_xml/`, build the class/method/enum model, then call `wx_gen_erl:gen/1` and `wx_gen_nif:gen/1`.
- `wx_gen_erl.erl` — emits the Erlang wrapper modules into `../src/gen/` (`<Class>.erl`, `wx_misc.erl`, `wxe_funcs.hrl`) and the public header `../include/wx.hrl`.
- `wx_gen_nif.erl` — emits the C++ glue into `../c_src/gen/` (`wxe_wrapper_<N>.cpp`, `wxe_func_table.cpp`, `wxe_init.cpp`, `wxe_macros.h`, `wxe_derived_dest.h`, `wxe_events.cpp`).
- `wx_gen_doc.erl` — doc helpers (`module_doc/1`, `func_doc/2`) used by `wx_gen_erl` to inject `-moduledoc` / `-doc`; its own `gen/1` entry is currently disabled.
- `wx_gen.hrl` — shared records (`#class{}`, `#method{}`, `#param{}`, `#type{}`, `#enum{}`, `#const{}`) and error/warning macros.
- `wxapi.conf` — the human-maintained spec: which classes, methods, enums, constants and events to expose, with per-item options (`alias`, `skip`, `taylormade`, `event`, `acc`, version gates via `{test_if, "wxCHECK_VERSION(...)"}`).
- `wx_doxygen.conf` — doxygen config for wx; `INPUT = $(WX_DIR)/interface ./wx_extra`, `XML_OUTPUT = ./wx_xml`.

### gl (OpenGL) generator suite
- `gl_gen.erl` — driver; entry point `code/0`.
  Consult `glapi.conf`, parse the doxygen XML in `gl_xml/` (`gl_8h.xml`, `glext_8h.xml`, `glu_8h.xml`), then drive the erl/nif/doc generators.
- `gl_gen_erl.erl` — emits `../src/gen/gl.erl`, `../src/gen/glu.erl`, `../src/gen/gl_debug.hrl` and the public headers `../include/gl.hrl`, `../include/glu.hrl`.
- `gl_gen_nif.erl` — emits `../c_src/gen/gl_nif.cpp`, `glu_nif.cpp`, `gl_fdefs.h`, `gl_finit.h`.
- `gl_gen_doc.erl` — scans the Khronos OpenGL-Refpages (`GL_MAN_SRC_DIR`) and stores function docs in the process dictionary before erl generation.
- `gl_scan_doc.erl` — SAX parser for the Khronos Refpage XML into doc strings; `gl_gen_doc` calls it.
- `gl_gen.hrl` — shared records (`#func{}`, `#arg{}`, `#type{}`, `#def{}`).
- `glapi.conf` — the human-maintained spec for the GL/GLEXT/GLU API (skip/keep lists, function options).
- `gl_doxygen.conf` — doxygen config for GL; `@GL_DIR@` is substituted with `GL_DIR` by the Makefile before running doxygen; `XML_OUTPUT = ./gl_xml/`.

### Shared / tooling
- `gen_util.erl` — helpers shared by both suites (tokenizer, name case conversion, arg formatting, `w/1,2` writer, copyright-header emitters).
- `apidiff.escript` — standalone `escript` that diffs two generated API footprints (`wx_28_api.dump` vs `wx_30_api.dump`) to flag compatibility breaks after regeneration.
- `Makefile` — orchestrates both suites (see Notes for Agents).
- `README` — the canonical usage/reproducibility notes.

## Generated Output (all checked into git — never hand-edit)
- `../src/gen/*.erl`, `../src/gen/wxe_funcs.hrl`, `../src/gen/gl_debug.hrl`, `../src/gen/vendor.info`.
- `../c_src/gen/*` (C++ NIF glue).
- `../include/wx.hrl`, `../include/gl.hrl`, `../include/glu.hrl`.
- `wx_30_api.dump` — API footprint written by `wx_gen_erl:gen_api_footprint/1`; consumed by `apidiff.escript` (gitignored).

## Constraints
- Generated files are checked in and marked `DO NOT EDIT`; a real change means editing `wxapi.conf` / `glapi.conf` / `wx_extra/` and re-running the generator, then committing the regenerated output.
- Never hand-edit anything under `../src/gen/`, `../c_src/gen/`, or the doxygen XML dirs `wx_xml/`, `gl_xml/`.
- The generator emits via `gen_util:open_write/1` + `gen_util:close/0`, which diffs the freshly generated text against the existing file and only rewrites it when the change is real (copyright-year-only diffs are discarded) — regeneration is idempotent and diff-stable.
- Names are convention-driven: override/extra files are only picked up when their base name matches a class/method known to the `.conf` spec.

## Design Decisions
- Spec-driven, not whole-header: the `.conf` files curate a stable, hand-reviewed subset of the enormous wxWidgets/OpenGL API surface (deliberately excluding unknown/incompatible functions).
- Two independent suites (wx, gl) with parallel structure and a shared `gen_util`, because the two upstream APIs (classes+methods vs flat C functions) have different shapes.
- Two-stage doxygen-XML → Erlang keeps C++ parsing out of Erlang: doxygen normalizes the headers, and the generators work on the stable XML model.
- Hand-written escape hatches live in `wx_extra/` and are merged at generation time so the generated output stays reproducible and the `DO NOT EDIT` headers stay truthful.
- Copyright headers are emitted by `gen_util` with per-class start years, so regenerated files keep stable license text.

## Known Issues
- The pipeline is version- and environment-sensitive: it needs doxygen 1.8.17 and Ericsson's patched wxWidgets checkout, so regeneration is not reproducible from a stock wxWidgets release.
- `gl_doxygen.conf` has a hardcoded `@GL_DIR@` placeholder that the Makefile must `sed`-substitute; running doxygen on it directly fails.
- OpenGL docs are silently skipped (not an error) when `GL_MAN_SRC_DIR` is unset — generated `gl.erl` then lacks `-doc` attributes.

## Notes for Agents
- To regenerate (wx): run `make` here with `WX_DIR=<wxWidgets src>`; targets `wxxml_generated` (doxygen) → `wx_code_generated` (`erl -noshell -run wx_gen code`).
- To regenerate (gl): `make` with `GL_DIR=<OpenGL headers>`; targets `glxml_generated` (sed + doxygen) → `gl_code_generated` (`erl -noshell -run gl_gen code`).
- `make opt` (default) runs both; `make complete_clean` also removes the doxygen XML and the `*_generated` stamps; plain `make clean` only removes the compiled generator `.beam` files.
- `GL_DIR` defaults to `/usr/include/GL`, `WX_DIR` defaults to `$(ERL_TOP)/../wxWidgets`.
- In `lib/wx/Makefile`, `api_gen` is only listed in `CLEANDIRS` — the normal OTP build never invokes generation.
- To change the exposed wx API: edit `wxapi.conf` (and add a `wx_extra/` override if generation can't express it), re-run `make`, then run `apidiff.escript` to check for compatibility breaks.
- The generators load `wxapi.conf` / `glapi.conf` with `file:consult/1` (Erlang term syntax) — keep them valid Erlang terms.

## Dependencies
- doxygen 1.8.17 (exact version required) — parses headers into `wx_xml/` / `gl_xml/`.
- A wxWidgets source checkout on Ericsson's patched `wx_api_gen` branch (`WX_DIR`) — the header patches are required for generation.
- OpenGL headers: mesa's `gl.h` plus `glext.h`/`glu.h` from opengl.org (`GL_DIR`); version recorded via `GL_GLEXT_VERSION` in `gl.hrl`.
- Khronos OpenGL-Refpages checkout (`GL_MAN_SRC_DIR`, from github.com/KhronosGroup/OpenGL-Refpages) — required for OpenGL `-doc` generation.

## Routing Table
- `wx_extra/` → hand-written Erlang/C++ overrides and extra doxygen headers merged into the generated wx output (see its own CONTEXT.md).
- `wx_xml/` → doxygen XML output for wxWidgets (generated; gitignored except an empty `.gitignore` placeholder).
- `gl_xml/` → doxygen XML output for OpenGL (generated; gitignored except an empty `.gitignore` placeholder).

## See Also
- `../src/gen/`, `../c_src/gen/`, `../include/` — consumers/recipients of the generated output.
- `../c_src/` — hand-written NIF driver that compiles the generated glue; `../src/` — hand-written wx runtime (`wxe_util`, `wx_object`, ...).
- `../../../scripts/` — `license-header.es`, which reads the `wx_extra/*.license` sidecars.
