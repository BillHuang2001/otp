# SSH — Secure Shell (OTP application `ssh`)

## Intent
Implements SSH-2 (RFC 4251-4256) as both client and daemon, plus an SFTP client (`ssh_sftp`) and SFTP subsystem server (`ssh_sftpd`).
Provides remote login (shell/CLI), remote exec, SFTP file transfer and TCP/IP port forwarding over an encrypted, authenticated transport.
Versioned by `vsn.mk` (`SSH_VSN`); `src/ssh.app.src` is the application resource (module list, `applications`, `env`, `runtime_dependencies`).
Strictly layered implementation: transport (RFC 4253) → authentication (RFC 4252) → connection protocol (RFC 4254), one module per layer, all driven by a single connection process.
Client and server share nearly all code — the role is a state in `#ssh{}` and in the FSM state name, not a separate implementation.

## API Surface
- `src/ssh.erl` — the public API and the only place most users should call.
  - Daemon: `daemon/1,2,3`, `daemon_info/1,2`, `daemon_replace_options/2`, `stop_listener/1,2,3`, `stop_daemon/1,2,3`.
  - Client: `connect/2,3,4`, `close/1`, `shell/1,2,3`, `connection_info/1,2`, `channel_info/3`.
  - Sockets/algorithms: `set_sock_opts/2`, `get_sock_opts/2`, `default_algorithms/0`, `chk_algos_opts/1`, `hostkey_fingerprint/1,2`.
  - Tunnels: `tcpip_tunnel_to_server/5,6`, `tcpip_tunnel_from_server/5,6`.
  - App lifecycle: `start/0,1`, `stop/0`.
  - Exported types are the contract for all option lists: `client_options()/daemon_options()/common_options()`, `daemon_ref()`, `connection_ref()`, `channel_id()`, algo-class types (`kex_alg()`, `pubkey_alg()`, `cipher_alg()`, `mac_alg()`, `compression_alg()`), plus deprecated aliases `ssh_daemon_ref()/ssh_connection_ref()/ssh_channel_id()`.
- `src/ssh_connection.erl` — public per-channel Connection Protocol API used by channel processes and users: `session_channel/2,4`, `exec/4`, `shell/2`, `subsystem/4`, `send/3,4,5`, `send_eof/2`, `adjust_window/3`, `setenv/5`, `close/2`, `reply_request/4`, `ptty_alloc/3,4`.
  Remote events reach channel processes as `{ssh_cm, ConnectionRef, ChannelMsg}`.
- Channel/subsystem behaviours (implement these to write custom subsystems):
  - `src/ssh_client_channel.erl` — client-side channel (`gen_server`-like: `init/1`, `handle_call/3`, `handle_cast/2`, `handle_msg/2`, `handle_ssh_msg/2`, `code_change/3`, `terminate/2`); also owns the internal channel cache helpers (`cache_create/0`, `cache_lookup/2`, `cache_insert/3`, `cache_update/2`).
  - `src/ssh_server_channel.erl` — server-side channel (`init/1`, `handle_msg/2`, `handle_ssh_msg/2`, `terminate/2`); the documented replacement for `ssh_daemon_channel`.
  - `src/ssh_channel.erl` and `src/ssh_daemon_channel.erl` — deprecated predecessors, kept for compatibility only (`-moduledoc false`).
- Key/storage behaviours (set via the `key_cb` option):
  - `src/ssh_client_key_api.erl` — `is_host_key/4,5`, `user_key/2`, `add_host_key/3,4`, optional `sign/3`; option type `client_key_cb_options/1`.
  - `src/ssh_server_key_api.erl` — `host_key/2`, `is_auth_key/3`; option type `daemon_key_cb_options/1`.
  - `src/ssh_file.erl` — default implementation, byte-compatible with OpenSSH file layout (`~/.ssh/id_*`, `known_hosts`, `authorized_keys`, `/etc/ssh/ssh_host_*`); public helpers `decode/2`, `encode/2`, `extract_public_key/1`.
  - `src/ssh_agent.erl` — `ssh_client_key_api` callback module talking to an SSH agent over a UNIX socket (`SSH_AUTH_SOCK` or `{socket_path, Path}` option).
- SFTP: `src/ssh_sftp.erl` (client, behaviour `ssh_client_channel`; `start_channel/1,2,3`, `stop_channel/1`, full file API) and `src/ssh_sftpd.erl` (server subsystem, `subsystem_spec/1`) with pluggable backend behaviour `src/ssh_sftpd_file_api.erl` and default backend `src/ssh_sftpd_file.erl`; wire protocol in `src/ssh_xfer.erl` + `src/ssh_xfer.hrl`.
- Debug/inspection helpers: `src/ssh_dbg.erl` (optional dbg-based tracing; also defines the `ssh_dbg` behaviour whose callbacks most modules implement), `src/ssh_info.erl` (`print/0,1`, `string/0`, `get_subs_tree/1` to dump a running instance's supervision tree).
- `src/ssh.app.src` — application resource (vsn from `%VSN%`, module list, `applications`, `env`, `runtime_dependencies`); `src/ssh.appup.src` — release upgrade/downgrade instructions, also `%VSN%`-substituted.

## Constraints
- `src/` is a single flat directory — no subdirectories, no public `include/`: the `*.hrl` files are internal (included with `-include("ssh.hrl")`) and are installed under `<release>/lib/ssh-<vsn>/src`, not exported to other apps.
- In-app layering is one-way: `ssh.erl` (API) → `ssh_connection_handler` → `ssh_transport`/`ssh_auth`/`ssh_connection` → `ssh_message`/`ssh_bits`; channel processes only talk to the connection through `ssh_connection` and the `{ssh_cm, _, _}` message protocol.
- Cross-app dependencies are limited to `crypto`/`public_key` (and `runtime_tools` for the debug/inspection helpers); do not add new application deps (in particular not `ssl`/`sasl`).
- Compile order inside `src/Makefile` matters: `BEHAVIOUR_MODULES_1` (`ssh_dbg`) then `BEHAVIOUR_MODULES_2` (key/channel behaviours) compile before `MODULES`, because callback modules reference them; keep new behaviour modules in those lists.
- Never edit `src/ssh.app` / `src/ssh.appup` (generated by `sed` from the `.src` files); the version lives only in `vsn.mk`.
- `INTERNAL_HRL_FILES` in `src/Makefile` must list every new internal `.hrl`, otherwise it is missing from the installed release; test-compilation headers are listed separately in `test/Makefile` `HRL_FILES`.
- `ebin/` and `priv/` contain only `.gitignore` (build output / no runtime data).
- Options must be declared in `ssh_options.erl` (`default/1` per role) — `ssh.erl` only documents them as `client_options()/daemon_options()/common_options()` types.
- Sensitive material (keys, password args) must not leak into reports: the logger filter installed by `ssh_app` relies on the `filter_modules` env and `ssh_options:no_sensitive/2`.

## Design Decisions
- One `gen_statem` process per SSH connection: `src/ssh_connection_handler.erl` (`callback_mode` = `[handle_event_function, state_enter]`).
  Its own states are `{hello,Role}` (version exchange) and `{connected,Role}`, where `connected` delegates channel messages to `ssh_connection:handle_msg/4`.
  The kex/rekey and authentication phases live in separate callback modules reached via `{change_callback_module, ...}`: `ssh_fsm_kexinit`, `ssh_fsm_userauth_client`, `ssh_fsm_userauth_server`.
  Shared FSM state is the single `#data{}` record in `ssh_fsm.hrl`; negotiated connection state is the single `#ssh{}` record in `ssh.hrl`.
- All wire messages are records per RFC (`ssh_msg_*` in `ssh_transport.hrl`, `ssh_auth.hrl`, `ssh_connect.hrl`) and are encoded/decoded centrally in `src/ssh_message.erl`; `src/ssh_bits.erl` provides SSH primitive encodings (`mpint/1`, `name_list/1`, `random/1`).
- Supervision tree (diagram in `internal_doc/ssh_notes.md`): `ssh_sup` → `sshd_sup` (daemon side) and `sshc_sup` (client side).
  - Daemon: `sshd_sup` → `ssh_lsocket_sup` (short-lived listen-socket providers) and `ssh_system_sup`, one per address:port, which owns `ssh_acceptor_sup` (`ssh_acceptor` acceptors) plus per-connection `ssh_connection_sup` → `ssh_channel_sup` / `ssh_tcpip_forward_acceptor_sup`.
  - Client: `sshc_sup` → `ssh_connection_sup` per connection → `ssh_channel_sup`, `ssh_tcpip_forward_acceptor_sup`. There is deliberately no `ssh_system_sup` on the client side.
  - The server-side per-connection supervisor is `ssh_connection_sup`; the client/server distinction is expressed with `auto_shutdown` and significant-child flags rather than extra supervisors.
- Listen sockets are produced by a separate throwaway "lsocket provider" process (`ssh_lsocket`/`ssh_lsocket_sup`) so that a slow or failing bind never blocks the daemon supervisor; `ssh:update_lsocket/3` reuses/updates it.
- Options are declared once as an `option_declarations()` map in `src/ssh_options.erl` (per class `user_option`/`undoc_user_option`, with a `chk` validation fun and default) and processed uniformly by `handle_options/2`; algorithm preference is finalized by merging `modify_algorithms` into `preferred_algorithms` (`final_preferred_algorithms/1`), with defaults from `ssh_transport:default_algorithms/0` plus the `preferred_algorithms` / `cnf_key(Role)` application env.
- Terminal/shell handling is isolated in `ssh_cli` (server-side CLI over a `group`-based pseudo terminal, `erlang_eval` exec) and `ssh_shell` (client-side interactive shell, which implements the *server*-channel behaviour because it handles remote requests).
- `ssh_dbg` is a behaviour implemented by most internal modules so trace points can be switched on per subsystem; tracing is off unless `ssh_dbg:start/0,1` + `ssh_dbg:on/0,1` are called.
- Port forwarding has symmetric client/server channel implementations (`ssh_tcpip_forward_client`, `ssh_tcpip_forward_srv`) plus a dynamic listener supervisor (`ssh_tcpip_forward_acceptor`/`_sup`).

## Known Issues
- Only `ssh`, `ssh_connection`, `ssh_file`, `ssh_agent`, `ssh_sftp`, `ssh_sftpd`, and the behaviour modules have published docs; the rest are `-moduledoc false` internals, so API changes there need no doc update but also give users no stability guarantee.
- `ssh_channel` and `ssh_daemon_channel` are deprecated aliases retained for backwards compatibility; new code must use `ssh_client_channel`/`ssh_server_channel`.
- Interactive prompting is done through an internal `io_cb` option resolved from the user-facing `user_interaction` (alias `allow_user_interaction`, default `true`) option: `ssh_io` prompts on stdin/stdout, `ssh_no_io` throws instead — services with no user present must set `{user_interaction, false}`.
- The SSH hello/version string and pre-auth message-size limits are guarded by explicit macros in `ssh.hrl` (e.g. `?MAX_DISCONNECT_DESC_SIZE`); touching size limits has interop implications with OpenSSH.

## Dependencies
- Erlang apps: kernel, stdlib, crypto, public_key (runtime_tools only for debug/inspection helpers) — see `src/ssh.app.src` `applications`/`runtime_dependencies`.
- External: no system libraries; key file formats and default paths mirror OpenSSH (`/etc/ssh`, `~/.ssh`), and agent support requires OpenSSH-compatible agent sockets.
- Build: plain OTP make (`Makefile` → `src`, `doc`), no autoconf/`configure` in this app; `src/Makefile` depends on `make/dep.mk` and `make/app_targets.mk`.

## Test Strategy
- `make tests` / `make test` run the Common Test suites in `test/`; see `test/CONTEXT.md` for suites, spec/cover files, helper libraries and external requirements (openssh, docker, QuickCheck).
- Interoperability with OpenSSH is covered by `ssh_to_openssh_SUITE` (local openssh binaries) and `ssh_compat_SUITE` (docker-based OpenSSH version matrix).
- Property-based models live in `test/property_test/` and are driven by `ssh_property_test_SUITE` (QuickCheck license required).

## Notes for Agents
- Distribution of options/config: defaults in `ssh_options:default/1`, algorithm defaults in `ssh_transport:default_algorithms/0` (env `preferred_algorithms`), everything user-visible documented in `ssh.erl` types and `doc/guides/configurations.md` — keep the three in sync.
- To add a channel/subsystem type, implement `ssh_server_channel` (daemon) or `ssh_client_channel` (client) and register it with the `subsystem`/`shell`/`exec` daemon options; the process is then started under `ssh_channel_sup`.
- Large modules: `ssh_connection_handler` (~2.6k lines), `ssh_transport` (~2.6k), `ssh_sftp` (~2.1k), `ssh_connection` (~2k) — read the FSM state functions in the `ssh_fsm_*` modules before changing protocol logic.
- `internal_doc/ssh_notes.md` is the fastest way to understand the supervision tree and also lists which test suite covers which concern.

## Routing Table
| Path | Description |
| --- | --- |
| `src/` | All Erlang sources (flat) — public API `ssh`, `ssh_connection`, behaviours, transport/auth/connection layers, SFTP, supervisors, headers (`*.hrl`), `ssh.app.src`/`ssh.appup.src` |
| `test/` | Common Test suites, support modules, fixtures and spec/cover files (see its CONTEXT.md) |
| `doc/` | Published documentation sources: app overview, release notes, six guides (see its CONTEXT.md) |
| `internal_doc/` | Non-published notes — `ssh_notes.md`: mermaid supervision-tree diagrams and the suite-coverage overview |
| `examples/` | Example code: `ssh_sample_cli.erl` (custom CLI/shell on a daemon) and `ssh_device.erl` (SSH as a device/transport); the local `Makefile` has release targets but the directory is not in `SUB_DIRECTORIES`, so it is not built or installed by the app build |
| `priv/` | Empty (`.gitignore` only) — no runtime data files |
| `vsn.mk` | `SSH_VSN` — the single source of the app version |
| `Makefile` | App build driver (`SUB_DIRECTORIES = src doc`, `DIA_PLT_APPS=crypto runtime_tools public_key`, `SPECIAL_TARGETS` empty) |
| `subdirs.mk` | Vestigial: `SUB_DIRS = src c_src`, but nothing includes it (`SUB_DIRECTORIES` is set directly in `Makefile`) and no `c_src` exists |
| `prebuild.skip` | Lists `src/deps/ssh.d` — generated dependency file excluded from source tarballs by `scripts/build-otp-tar` |

## See Also
- `../public_key/`, `../crypto/` — key handling and cryptographic primitives used by the transport layer.
- `../../erts/` — socket primitives; `../kernel/` — `inet`/`logger` integration.
- `../../CONTEXT.md`, `../CONTEXT.md` — repo-level and OTP-library build conventions (app layout, `vsn.mk`, `*.app.src`).
