%%% Oracle fixture for the OTP-29 **native-record** opcode family (186-193).
%%%
%%% `nr_def` *defines* and exports a native record (`-record #rec{a, b}.` plus
%%% `-export_record([rec])`), so its `Recs` chunk carries `rec` with
%%% `is_exported = true`. Its only function `mk/0` builds one, which the compiler
%%% lowers to `put_record/6` (opcode 189) with the local record identity atom.
%%%
%%% Compiled together with `nr_use.erl` by the system `erlc`; see
%%% `tests/fixtures/README.md` for the exact regeneration command.
-module(nr_def).
-export([mk/0]).
-export_record([rec]).
-record #rec{a, b}.
mk() -> #rec{a = 1, b = 2}.
