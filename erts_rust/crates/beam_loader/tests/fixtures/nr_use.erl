%%% Oracle fixture for the OTP-29 **native-record** opcode family (186-193).
%%%
%%% `nr_use` imports the `rec` native record from `nr_def`, defines its own local
%%% record `#local{a, b}`, and exercises the whole OTP-29 subset:
%%%
%%%   * `local_mk/0`  -> `put_record` (189), local identity atom `local`
%%%   * `local_peek/1` -> `is_any_native_record` (186), `is_native_record` (187),
%%%                       `get_record_elements` (188)
%%%   * `local_fld/1` -> `get_record_field` (191)
%%%   * `def_mk/0`    -> `put_record` (189) with a `{recdef, rec}` identity literal
%%%   * `def_upd/1`   -> `is_native_record` (187), `is_record_accessible` (190),
%%%                       `put_record` (189)
%%%   * `def_fld/1`   -> `get_record_field` (191) with a `{recdef, rec}` identity
%%%
%%% Compiled together with `nr_def.erl` by the system `erlc`; see
%%% `tests/fixtures/README.md` for the exact regeneration command.
-module(nr_use).
-export([local_mk/0, local_peek/1, local_fld/1, def_mk/0, def_upd/1, def_fld/1]).
-import_record(nr_def, [rec]).
-record #local{a, b}.
local_mk() -> #local{a = 1, b = 2}.
local_peek(R) -> #local{a = A, b = B} = R, {A, B}.
local_fld(R) -> R#local.a.
def_mk() -> #recdef:rec{a = 1, b = 2}.
def_upd(R) -> R#recdef:rec{a = 7}.
def_fld(R) -> R#recdef:rec.a.
