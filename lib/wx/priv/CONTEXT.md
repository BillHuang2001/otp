# wx — Private Runtime Assets

## Intent
`priv/` holds the `wx` application's runtime data files (the standard OTP `priv` directory).
At build/install time the compiled NIF shared libraries (`wxe_driver.<so|dll>`, `erl_gl.<so|dll>`, and on Windows `WebView2Loader.dll`) are also placed here by `../c_src/`.

## API Surface
- `erlang-logo32.png`, `erlang-logo64.png`, `erlang-logo128.png` — the Erlang logo images used by wx demos/art (installed to `$(RELSYSDIR)/priv/` by `../c_src/Makefile.in`).
- `*.png.license` — SPDX license sidecars for the images (consumed by the license-header tooling).

## Constraints
- The checked-in files here are only the images; the built NIF libraries in this directory are git-ignored build output (per-platform, e.g. `priv/<sys_type>/`).

## See Also
- `../c_src/` — produces and installs the NIF libraries into this directory.
- `../src/wxe_util.erl` — locates the driver via `priv_dir/2` (`code:priv_dir(wx)`, with an arch-specific `priv/<arch>/` fallback).
