# snmp/src/misc — Shared SNMP Modules

## Intent
Code shared by both the SNMP agent and manager sides: PDU encode/decode, config-file syntax and config-data handling, USM (User-based Security Model) helpers, logging/verbosity, note storage, and low-level utilities.
Built as one of the `SUB_DIRECTORIES = compile app misc agent manager` of `src/Makefile`; its modules are installed into `<release>/lib/snmp-<vsn>/ebin` and its sources+headers into `.../src/misc`.

## API Surface
- `snmp_pdus.erl` — the SNMP PDU codec: `enc_message/1`, `enc_message_only/1`, `enc_pdu/1`, `enc_scoped_pdu/1`, `enc_usm_security_parameters/1`, `enc_varbind/1`, `dec_message/1`, `dec_pdu/1`, `dec_scoped_pdu/1`, `dec_value/1`, `get_encoded_length/1`, plus exported types (`message/0`, `pdu/0`, `scoped_pdu/0`, …). Public API documented in `-moduledoc`; records come from `include/snmp_types.hrl` (defines `SNMP_USE_V3` before including to get v3 forms). Uses `snmp_pdus_basic` for Counter64.
- `snmp_pdus_basic.erl` — **GENERATED** (build-time) from the hand-crafted ASN.1 spec `snmp_pdus_basic.asn` / `snmp_pdus_basic.set.asn` via the `asn1` app (`erlc -bber +noobj`). Provides `encode/2` / `decode/2` for the SNMPv2-SMI basic integer types (Counter32, Unsigned32, TimeTicks, Counter64). Currently only `decode('Counter64',…)`/`encode('Counter64',…)` are consumed, by `snmp_pdus`.
- `snmp_conf.erl` — config-file syntax helpers: `read_files/2`, `read/2`, `read/3`, generators/order/filter hooks (`no_gen`, `no_order`, `no_filter`, `keyorder`) and the type-check functions (`check_ip`, `check_domain`, `check_tdomain`, `check_sec_model`, …). `-moduledoc false`.
- `snmp_config.erl` — config-handling API over the config data model: `config/0`, `write_config_file/6`/`append_config_file/6`/`read_config_file/4`, and the `write_agent_*_config` / `update_agent_*_config` / `write_manager_*_config` families (delegating to `snmpa_conf`/`snmpm_conf`). `-moduledoc false`.
- `snmp_log.erl` — SNMP log (disk_log-backed) management: `open/1,2`, `create/4,5,6`, `log/3,4`, `close/1`, `sync/1`, `info/1`, `log_to_txt/*`, `log_to_io/*`, `upgrade/*`/`downgrade/1`, `validate/*`; opaque `log/0` type.
- `snmp_misc.erl` — assorted helpers used across the app: `format_pdu/2`, `format_vb(s)/2`, `format/3`, `format_*timestamp`, `read_mib/1`, `strip_extension_from_filename/2`, `mk_msg_flags/2`, `get_sec_level/1`, `is_auth/1`, `is_priv/1`, `is_oid/1`, `assq/2`, `keyreplaceadd/4`, `mem_size/1`, `time/3`, `sleep/1`, `now/1`, `str_xor/2`, `verify_behaviour/2`, and option helpers.
- `snmp_mini_mib.erl` — in-memory "mini MIB" built from compiled `.bin` MIBs: `create/1`, `delete/1`, `oid/2`, `aliasname/2`, `type/2` (loads `.bin` via `snmp_misc:read_mib/1`; used by `snmp_log`).
- `snmp_note_store.erl` — a `gen_server` note store with per-note lifetime and periodic GC: `start_link/3`, `stop/1`, `get_note/2`, `set_note/3,4`, `info/1`, `verbosity/2`.
- `snmp_usm.erl` — User-based Security Model primitives: `passwd2localized_key/3`, `localize_key/3`, `auth_in/4`, `auth_out/4`, `set_msg_auth_params/3`, `des_encrypt/3`, `des_decrypt/3`, `aes_encrypt/5`, `aes_decrypt/5`.
- `snmp_verbosity.erl` — verbosity printing backend: `print/4,5`, `printc/4`, `validate/1`, `process_args/2`; consumed via the macros in `snmp_verbosity.hrl` (51 files reference it).
- Exported headers: `snmp_usm.hrl` (RFC 7860 secret-key/MAC lengths for HMAC-SHA-2 auth protocols), `snmp_verbosity.hrl` (`?vinfo`/`?vlog`/`?vdebug`/`?vtrace`/`?vvalidate` macros; no-ops when `dont_use_verbosity` is defined), `snmp_debug.hrl` (`?d`/`?d_b`/`?d_e` debug macros; no-ops unless `snmp_debug` is defined — internal debug only).

## Constraints
- `snmp_pdus_basic.erl` and `snmp_pdus_basic.asn1db` are GENERATED and git-ignored (`src/.gitignore`) — never edit them; edit `snmp_pdus_basic.asn` (the ASN.1 spec) or `.set.asn` (the file list) instead. `make clean` removes the generated `.erl`/`.asn1db`.
- Compiled with `-Werror` (unless `SNMP_WARNING_AS_ERROR=false`) and `+warn_unused_vars` (unless `WARN_UNUSED_VARS=false`).
- Headers are exported for other dirs: `agent/`/`manager/`/`app/` add `-I../misc` (and `manager/` uses `-include_lib("snmp/src/misc/snmp_debug.hrl")`) to consume `snmp_verbosity.hrl`, `snmp_usm.hrl`, `snmp_debug.hrl`; `snmp_misc.erl` also includes `../compile/snmpc_misc.hrl`.
- Verbosity is compiled in via `-Ddefault_verbosity=<level>` (`SNMP_DEFAULT_VERBOSITY`, default `silence`); debug macros require `SNMP_DEBUG=d` (`-Dsnmp_debug`).

## Design Decisions
- `snmp_pdus` is hand-written for full control of the wire codec; a subset of basic SNMPv2-SMI integer types is instead generated from a hand-crafted ASN.1 spec (`snmp_pdus_basic`) as future-proofing for a binary-based codec — currently only Counter64 uses it.
- `snmp_conf` handles file *syntax* (reading/parsing/formatting entries with pluggable Gen/Order/Check/Filter hooks), while `snmp_config` handles the *config data model* (writing/updating agent & manager config files, delegating app-specific writers to `snmpa_conf`/`snmpm_conf`).
- Header macros (`snmp_verbosity.hrl`, `snmp_debug.hrl`) compile to no-ops in release builds so logging is zero-cost unless enabled at compile time.

## Notes for Agents
- There are no subdirectories here; all modules live flat in this directory.
- `-moduledoc false` on most modules means they are internal — only `snmp_pdus` is documented for end users.

## Dependencies
- Build requires the `asn1` app (for the generated `snmp_pdus_basic`); runtime crypto for USM uses `crypto`.
- `snmp_misc`/`snmp_mini_mib` depend on the compiled-MIB format produced by `../compile/`.

## See Also
- `../agent/` (`snmpa_*` consumers), `../manager/` (`snmpm_*`), `../compile/` (compiled MIBs consumed at runtime and `snmpc_misc.hrl`), `../app/` (`snmp.app.src`, `snmp.erl`). Read-only siblings; escalate writes to `./lib/snmp`.
