# wx API Generator — Hand-Written Extras (`wx_extra/`)

## Intent
Hand-maintained source that the wx API generator (`../wx_gen*.erl`) merges into the code it generates, for cases doxygen-derived generation cannot express (callback plumbing, derived C++ classes, methods missing from or removed in wxWidgets headers).
This is the only hand-edited directory under `lib/wx/api_gen/`; everything it feeds (`../wx_xml/`, `../src/gen/`, `../c_src/gen/`) is generated output.

## API Surface

### 1. Doxygen inputs (extra C++ declarations)
- `added_func.h` — fake C++ declarations for classes/methods absent from the wxWidgets headers doxygen parses (e.g. `wxXmlResource::xrcctrl`, `wxTreeCtrlBase::IsTreeItemIdOk`, `wxMenuBar::SetAutoWindowMenu`, `wxListItemAttr`, `wxAuiNotebookEvent::SetDragSource`, `wxGLCanvas::CreateSurface`), so they show up in `../wx_xml/` and can be generated.
- `wxe_evth.h` — declaration plus doxygen documentation for the internal `wxeEvtHandler` class (`Connect`, `Disconnect`); this is the class behind the `wxEvtHandler` Erlang module and the parent of `wxWindow`.
- `../wx_doxygen.conf` lists `./wx_extra` in `INPUT` with `FILE_PATTERNS = *.h *.md`, so only these `.h` files are scanned — the `.erl`/`.c_src` files are invisible to doxygen.

### 2. Generator overrides — one file pair per class
Erlang side `wx_extra/<Module>.erl`, C side `wx_extra/<Class>.c_src`; `<Module>`/`<Class>` must be the name as it appears after `../wxapi.conf` alias resolution (e.g. `wxEvtHandler`, not the conf-internal `wxeEvtHandler`).
| Class | Files | What is overridden |
| --- | --- | --- |
| `wxeEvtHandler` → `wxEvtHandler` | `wxEvtHandler.erl`, `wxEvtHandler.c_src` | Whole-class taylormade: `connect/2,3` and `disconnect/1,2,3`, option parsing, `wxeEvtListener` bookkeeping — the whole module is hand-written (not in the generated chunk files, and the `.c_src` is copied verbatim). |
| `wxListCtrl` | `wxListCtrl.erl`, `wxListCtrl.c_src` | `new/0,1,2`, `create/2,3`, `sortItems/2`, plus derived `EwxListCtrl` with per-item callbacks (`onGetItemText`, `onGetItemAttr`, `onGetItemColumnImage`). |
| `wxPrintout` | `wxPrintout.erl`, `wxPrintout.c_src` | `new/2,3` carrying 8 print callback funs and the derived `EwxPrintout`. |
| `wxTaskBarIcon` | `wxTaskBarIcon.erl`, `wxTaskBarIcon.c_src` | `new/0,1` with the `createPopupMenu` callback and derived `EwxTaskBarIcon`. |
| `wxXmlResource` | `wxXmlResource.erl`, `wxXmlResource.c_src` | `xrcctrl/3` — implemented on the Erlang side (`getXRCID` + `wxWindow:findWindow` + `wx:typeCast`), so the C chunk is a comment-only stub. |
| `wxTreeCtrl` | `wxTreeCtrl.c_src` only | `IsTreeItemIdOk/1` NIF; the Erlang wrapper is generated normally. |
| `wxGraphicsRenderer` | `wxGraphicsRenderer.c_src` only | Contains a `<<CreatePen ...>>` chunk (currently unused — see Known Issues). |
| (static functions) | `wx_misc.erl` | Hand-written Erlang appended verbatim to generated `../src/gen/wx_misc.erl`; declares its own `-export` (e.g. `mSWSetEmulationLevel/1,2`). |

### 3. `.license` sidecars
`<file>.license` siblings exist only for files that carry no in-file `%CopyrightBegin%` header: `wxEvtHandler.erl`, `wxEvtHandler.c_src`, `wx_misc.erl`, `wxPrintout.c_src`, `wxXmlResource.c_src`.
They are read by `../../../scripts/license-header.es` (`scripts/license-header.es` from the repo root) as the header source for that file, only when the file itself has no header.
Editing a `.license` file affects the license check only, never code generation.

## Marker Contract (how the generator consumes these files)
The mechanism is selected by `../wxapi.conf`, not by the file:

1. **Whole-class taylormade** — class option `taylormade` (only `wxeEvtHandler`).
   The entire `<Class>.erl` / `<Class>.c_src` is copied verbatim into the generated file; no markers are used.
   The `.erl` must therefore be a complete module (`-module`, `-export`, includes, docs); the generated file still gets the standard copyright block and `%% This file is generated DO NOT EDIT` preamble prepended.
   The class's method list in `wxapi.conf` is still used for inheritance and the C dispatch table, even though it does not drive Erlang generation.

2. **Per-method taylormade** — method option `{where, taylormade}`.
   The generator extracts named chunks `<<ChunkName ... ChunkName>>` and injects each at that method's position in the generated wrapper:
   - `ChunkName` is the Erlang method name (`Create`, `SortItems`, `xrcctrl`, `wxPrintout`) or, when that does not exist, the generator's unique op name `<Class>_<Method>` (e.g. `wxListCtrl_new_0`, `wxTreeCtrl_IsTreeItemIdOk`).
   - `~s` inside a chunk is substituted with the C op macro (`?wxListCtrl_new_0` in `.erl`) or the bare C function name (`wxListCtrl_new_0` in `.c_src`); a chunk may omit it.
   - A chunk may be an empty/comment-only stub to suppress code (e.g. `wxListCtrl_new_2` in `wxListCtrl.c_src` emits only `// skipped ~s`).
   - Exports come from `<<EXPORT:ChunkName foo/1, bar/2 ChunkName:EXPORT>>` in the `.erl`; if the `.erl` file is absent the generator derives the export/deprecation list from the XML instead.
   - A `<Class>_class` chunk in the `.c_src` replaces the generated derived `E<Class>` C++ class (emitted into `../c_src/gen/wxe_derived_dest.h`); per-method chunks land in `../c_src/gen/wxe_wrapper_<N>.cpp`.
   - `wx_misc.erl` is a special case: `../wx_gen_erl.erl` appends it verbatim to the generated static module (it is not chunked, because the static module has no class to key chunks by).

## Constraints
- These files are hand-maintained — never regenerate or overwrite them; the generated counterparts (`../src/gen/*.erl`, `../c_src/gen/*`, `../wx_xml/*`) are what get overwritten.
- Edits to `wx_extra/wx*.erl` or `wx_extra/wx*.c_src` invalidate `../Makefile`'s `wx_code_generated` target; edits to `wxe_evth.h`/`added_func.h` invalidate `wxxml_generated` and re-run doxygen; `.license` files are not build inputs.
- Regeneration needs doxygen 1.8.17 and a checkout of Ericsson's patched wxWidgets (`wx_api_gen` branch, `WX_DIR`); see `../README`. Generated output is checked in, so ordinary OTP builds never run the generator.
- File names are convention-driven, not configured: a file is picked up only if its base name matches a class/method name known to `../wxapi.conf`.
- Keep the doxygen comments in the `.h` files accurate — that text becomes the public documentation of the generated function.

## Design Decisions
- Overrides live here rather than being patched into generated files so regeneration stays idempotent and the `DO NOT EDIT` headers remain truthful.
- Whole-class taylormade is used only for the event handler, whose callback plumbing (fun ids, listener refs, delete-callback messages) is too intertwined with the generator's patterns to express per method.
- Classes that need a derived `E<Class>` (`wxListCtrl`, `wxPrintout`, `wxTaskBarIcon`) are overridden per method, so the bulk of their API (inherited and unmarked methods) is still generated automatically.
- Some overrides are C-only (`wxTreeCtrl`): only the NIF call is hand-written and the Erlang wrapper is generated; the generator tolerates a missing `<Class>.erl` by catching `{error, enoent}`.
- Callback-taking constructors keep their custom option marshalling in Erlang here (`onGetItemText`, `createPopupMenu`, print callbacks) because the generic option marshaller has no notion of `fun` → callback-id conversion.

## Known Issues
- `wxGraphicsRenderer.c_src` is not currently consumed: `wxGraphicsRenderer` has no `taylormade` option and no `{where, taylormade}` method in `../wxapi.conf`, and the generated `wxGraphicsContext_CreatePen` (`../c_src/gen/wxe_wrapper_3.cpp`) uses the generic `new wxGraphicsPen(This->CreatePen(*pen))` form instead of this file's version-dependent chunk.
- `wxEvtHandler.erl` opens with a stale `%% This module is actually handwritten see ../api_gen/wx_extra/wxEvtHandler.erl` comment that duplicates the generated preamble.
- A per-method override whose `<Class>.erl` exists but lacks the `<<EXPORT:...>>` chunk aborts generation with a `{badmatch, nomatch}` error; only a missing file is tolerated.

## Notes for Agents
- To add a per-method override: add `{where, taylormade}` to the method in `../wxapi.conf`, then add the `<<Name ... Name>>` chunk (plus, for the Erlang side, `<<EXPORT:Name ... Name:EXPORT>>`) to `wx_extra/<Module>.erl` and/or `wx_extra/<Class>.c_src`.
- To add a whole-class override: add `taylormade` to the class option list in `../wxapi.conf` and provide complete `<Class>.erl` / `<Class>.c_src` files (no markers).
- Use the generator's unique op name (`<Class>_<Method>`, as printed in `../c_src/gen/wxe_func_table.cpp`) as the chunk name for constructors, overloads and methods whose Erlang name is ambiguous.
- Never edit `../src/gen/`, `../c_src/gen/` or `../wx_xml/`; a fix belongs here or in `../wxapi.conf`.
- After editing, run `make` in `lib/wx/api_gen`, then commit the regenerated files (`lib/wx/src/gen/...`, `lib/wx/c_src/gen/...`, `lib/wx/api_gen/wx_xml/...`) together with the override.

## Dependencies
- Regeneration only: doxygen 1.8.17, a wxWidgets checkout on Ericsson's `wx_api_gen` branch (`WX_DIR`), and OpenGL headers (`GL_DIR`) for the separate GL generator.

## See Also
- `../CONTEXT.md` — generator sources (`wx_gen*.erl`), `wxapi.conf`, `wx_doxygen.conf`, and the generated output layout.
- `../../src/` and `../../c_src/` — consumers of the generated Erlang and C++ wrappers.
- `../../../scripts/` — `license-header.es`, the reader of the `.license` sidecars.
