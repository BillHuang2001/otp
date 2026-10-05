# inets Examples — sample `httpd` server root

## Intent
Ships a fully working *sample* `httpd` server root (`server_root/`) that demonstrates configuration and every major `inets` HTTP server feature: static files, aliases, CGI, ESI (`mod_esi`), server-side includes, and `mod_auth` with `plain`/`mnesia` backends.
It is both user documentation (installed with OTP so users can copy and adapt it) and the reference layout that the real test fixture mirrors.
`examples/` is built and released as part of the `inets` application but contains no Erlang code, only data/config that is copied on install.

## API Surface (layout inventory)
- `Makefile` — recurses only; includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk`, `subdirs.mk`, `make/otp_subdir.mk`; `SPECIAL_TARGETS` empty.
- `subdirs.mk` — `SUB_DIRECTORIES = server_root`.
- `server_root/Makefile` — declares the file lists (`AUTH_FILES`, `CGI_FILES`, `CONF_FILES`, `OPEN_FILES`, `MNESIA_OPEN_FILES`, `MISC_FILES`, `SECRET_FILES`, `MNESIA_SECRET_FILES`, `HTDOCS_FILES`, `ICON_FILES`, `SSL_FILES`) and installs them in `release_docs_spec` to `$(RELSYSDIR)/doc/examples/server_root/...` (`RELSYSDIR = $(RELEASE_PATH)/lib/inets-$(VSN)`, `VSN=$(INETS_VSN)`); `clean`/`docs`/`$(TYPES)` are no-ops.
- `server_root/conf/` — three alternative server configs plus the MIME table (see routing table).
- `server_root/htdocs/` — the `document_root`: `index.html` (links to all SSI + ESI demos), `last_modified.html`, `misc/` (fragments `friedrich.html`, `oech.html`, `welcome.html` used as SSI include targets / directory-index candidates), the six SSI demos (`config.shtml`, `echo.shtml`, `exec.shtml`, `flastmod.shtml`, `fsize.shtml`, `include.shtml`), and the access-controlled areas: `open/`, `secret/` (`secret/top_secret/`), `mnesia_open/`, `mnesia_secret/` (`mnesia_secret/top_secret/`), `dets_open/`, `dets_secret/` (`dets_secret/top_secret/`) — each a `dummy.html` (plus `index.html` in the `top_secret` dirs).
- `server_root/cgi-bin/` — `printenv.sh` (installed) and `printenv.bat` (Windows variant, not in `CGI_FILES`) CGI samples that dump the CGI environment.
- `server_root/auth/` — `passwd` (users `one`/`two`/`three`/`Aladdin`) and `group` (`group1`, `group2`, `group3`) files consumed by `mod_auth` `plain` backend via `auth_user_file`/`auth_group_file`.
- `server_root/icons/` — ~90 public-domain GIFs (originally for Mosaic/NCSA/Apache, see `icons/README`) served via the `/icons/` and `/pics/` aliases.
- `server_root/ssl/` — `ssl_server.pem` and `ssl_client.pem`, each a combined PEM certificate + RSA private key for the SSL example.
- `server_root/logs/` — empty directory (only the placeholder `Dummy_File_Needed_By_WinZip`) so the log paths in the configs resolve and so the empty dir survives zip packaging.

## What each config demonstrates
- `conf/httpd.conf` — port `8888`; fully commented reference for every `httpd` directive (`server_name`, `socket_type`, `modules`, `server_root`, error/transfer/security logs incl. `*_disk_log` variants, `max_clients`, `keep_alive*`, `document_root`, `directory_index`, `default_type`, `alias`, `script_alias`, `action`, `erl_script_alias`); includes commented-out SSL `socket_type` and `{action, ...}` examples; ends with `directory` blocks configuring `plain` auth for `open`/`secret`/`secret/top_secret` and `mnesia` auth (plus a `mod_security` block with `security_callback_module`, `deny_from`/`allow_from`) for `mnesia_*`.
- `conf/8080.conf` — compact variant on port `8080` with the same feature mix (`plain` + `mnesia` + `mod_security`), separate `*_8080` log files.
- `conf/ssl.conf` — port `8088` with `{socket_type, {ssl, [{certfile,...ssl_server_cert.pem}, {keyfile,...ssl_key.pem}, {verify, verify_none}]}}`; auth blocks use the older `{auth_mnesia_d_b,"On"}` flag form.
- `htdocs/*.shtml` — each single-feature demo of an SSI command: `echo` (`DATE_LOCAL`, `DOCUMENT_URI`, …), `include` (`file=` vs `virtual=`), `fsize`, `flastmod`, `exec` (`cmd=` and `cgi=`), and `config.shtml` which exercises `timefmt`/`sizefmt`/`errmsg` plus deliberate error cases (including the two `fsize` forms and undefined targets); served by `mod_esi`'s SSI handling.
- `htdocs/index.html` — links the SSI demos and the ESI callbacks `cgi-bin/erl/httpd_example/{get,yahoo,test1}`, which resolve through the `erl_script_alias` to the `httpd_example` module.

## Constraints
- This tree is *sample data* — never hand-edit anything under `server_root/` from a build/test process, and never add build outputs here.
- All config files use absolute `/var/tmp/server_root/...` paths (ServerRoot, document root, aliases, auth files), so the tree only works when copied/installed into that location or when the paths are edited; keep them consistent across the three configs.
- Config files must stay valid Erlang terms (they are parsed by `httpd_conf`); keep the trailing `.` and the `{directive, Value}` shape.
- The SSI/ESI demos reference the module `httpd_example` (ESI callback) and `security_callback` (`mod_security` callback) — those modules live in `../src/http_server/`, not here; do not expect local copies.
- New files added under `server_root/` must also be added to the relevant `*_FILES` list in `server_root/Makefile`, otherwise they will not be installed.

## Known Issues
- The `release_docs_spec` in `server_root/Makefile` installs only a subset of the in-tree files: the whole `dets_open/` and `dets_secret/` trees, `htdocs/last_modified.html`, `htdocs/misc/welcome.html`, and the `top_secret/index.html` files are never copied (only the `top_secret` directories are created).
- `conf/ssl.conf` points at `ssl/ssl_server_cert.pem` and `ssl/ssl_key.pem`, which do not exist — the tree ships combined `ssl/ssl_server.pem` and `ssl/ssl_client.pem` instead.
- The `dets_open/`/`dets_secret/` areas are not referenced by any config here (no `dets` auth example remains); they are orphaned sample directories.
- `auth/passwd` holds plaintext sample passwords and `ssl/*.pem` embed a sample self-signed private key — these are intentional demo values, not real secrets; keep them as-is rather than "obfuscating" them.

## Notes for Agents
- This directory carries no compiled code: `make` here just recurses, and installation happens from `release_docs_spec` (run as part of the app/release build), not from a default build target.
- In a released OTP the tree ends up under `$(RELEASE_PATH)/lib/inets-<vsn>/doc/examples/server_root/` (moved under `doc/` on release), while the source location stays `lib/inets/examples/server_root/`.
- `cgi-bin/printenv.sh` is installed with `INSTALL_SCRIPT` (executable bit), the rest with `INSTALL_DATA`.
- The `.bat` CGI variant and the stale `c:\cygwin\tmp\hej` echo line in it are Windows-only test leftovers.

## Routing Table
| Path | What it is / demonstrates |
| --- | --- |
| `Makefile`, `subdirs.mk` | build recursion + `SUB_DIRECTORIES` for this node |
| `server_root/Makefile` | file lists and release/install rules for the sample tree |
| `server_root/conf/` | `httpd` configuration examples (`httpd.conf`, `8080.conf`, `ssl.conf`) and `mime.types` — see `../../src/http_server/httpd_conf.erl` for directive parsing |
| `server_root/htdocs/` | document root: SSI demos (`*.shtml` → `mod_esi` SSI handling in `../../src/http_server/mod_esi.erl`), static pages, and the auth-protected areas (`open`/`secret`/`mnesia_*`/`dets_*`) |
| `server_root/cgi-bin/` | CGI sample (`printenv.sh`/`.bat`) — driven by `../../src/http_server/mod_cgi.erl` (`script_alias`) |
| `server_root/auth/` | `passwd`/`group` files for the `plain` backend — see `../../src/http_server/mod_auth_plain.erl`, `mod_auth.erl` |
| `server_root/icons/` | public-domain GIF set served via the `/icons/` alias — see `../../src/http_server/mod_alias.erl` |
| `server_root/ssl/` | sample certs for the SSL config — see `../../src/http_server/httpd_socket.erl` |
| `server_root/logs/` | placeholder dir for the configured log paths — see `../../src/http_server/mod_log.erl`, `mod_disk_log.erl` |
| `../../src/http_server/httpd_example.erl` | the ESI callback module referenced by `erl_script_alias` in all three configs |
| `../../src/http_server/mod_security*.erl` | implements the `security_callback_module` / `deny_from` directives used in `httpd.conf`/`8080.conf` |
| `../../test/httpd_test_data/server_root/` | CT fixture twin of this tree (sibling — read-only, escalate writes to the parent) |
