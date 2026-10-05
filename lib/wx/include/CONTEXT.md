# wx — Public Erlang Headers

## Intent
Public Erlang headers installed for the `wx` application, shared with user code and other applications.
All three headers are **generated** by `../api_gen/` and checked in.

## API Surface
- `wx.hrl` — the `#wx{}` event record plus one record + type per event class (`#wxButton`, `#wxActivate`, ...); includes the `?wx*` event-code macros. Consumed via `-include_lib("wx/include/wx.hrl")`.
- `gl.hrl` — generated OpenGL constant macros and version info (`GL_GLEXT_VERSION`).
- `glu.hrl` — generated GLU constant macros.

## Constraints
- Do NOT hand-edit: all files carry `%% This file is generated DO NOT EDIT`; regenerate via `../api_gen/` (`wx_gen_erl`/`gl_gen_erl`).
- These are the public headers; the private internal header is `../src/wxe.hrl` (not installed).
- Installed to `$(RELSYSDIR)/include` by `../src/Makefile`'s `release_spec`.

## See Also
- `../api_gen/` — generator that emits these headers (sibling, read-only).
- `../src/gen/` — the generated modules that define the records/types these headers reference.
