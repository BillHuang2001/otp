%% Exception-path oracle fixture (see ../exceptions.rs, the "shared-atom host"
%% section).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen" erlc +deterministic excpaths.erl`.
%%
%% ## Why this fixture exists
%%
%% The real OTP compiler does **not** emit the runtime's well-known exception
%% atoms (`function_clause`, `case_clause`, `if_clause`, `try_clause`,
%% `badmatch`, `error`, `'EXIT'`, `file`, `line`, ...) into a module's atom chunk,
%% because a real runtime keeps them globally interned
%% (`erts/emulator/beam/beam_common.c:382-406`, `error_atom[NUMBER_EXIT_CODES]`).
%% A module loaded on its own therefore cannot *name* its own exception reasons:
%% the engine, which resolves a reason atom by name from the running module's
%% table (and refuses to substitute a different atom), degrades those paths to a
%% typed `unsupported`. This fixture's atom chunk deliberately contains none of
%% them (verify with `beam_lib:chunks("excpaths.beam", [atoms])`), so the tests
%% can only pass when the host seeds the well-known atoms into every loaded
%% module — the host's analogue of the runtime's global interning.
%%
%% The module sticks to opcodes the Milestone-1 interpreter + families execute
%% (`func_info`, `case_end`, `if_end`, `try_case_end`, `bs_start_match3`,
%% `bs_get_position`, `bs_match`, `bs_set_position`, `bs_get_tail`, `badmatch`,
%% `try`/`try_case`/`try_end`, `catch`/`catch_end`, `build_stacktrace`,
%% `put_tuple2`, `init_yregs`, plain calls/returns).
%%
%% ## Derived with the oracle (system `erl`)
%%
%% ```text
%% excpaths:fc(b)          -> error:function_clause
%% excpaths:cc(b)          -> error:{case_clause,b}
%% excpaths:ii(-1)         -> error:if_clause
%% excpaths:bm(a)          -> error:{badmatch,a}
%% excpaths:tc(b)          -> error:{try_clause,b}
%% excpaths:catch_fc(b)    -> {error,function_clause}
%% excpaths:catch_cc(b)    -> {error,{case_clause,b}}
%% excpaths:catch_ii(-1)   -> {error,if_clause}
%% excpaths:catch_bm(a)    -> {error,{badmatch,a}}
%% excpaths:catch_tc(b)    -> {error,{try_clause,b}}
%% excpaths:catch_class(b) -> error
%% excpaths:oldcatch_bm(a) -> {'EXIT',{{badmatch,a},Stacktrace}}
%% excpaths:trace(b)       -> {error,{case_clause,b},Stacktrace}
%% ```
-module(excpaths).
-export([fc/1, cc/1, ii/1, bm/1, tc/1,
         catch_fc/1, catch_cc/1, catch_ii/1, catch_bm/1, catch_tc/1,
         catch_class/1, oldcatch_bm/1, trace/1]).

%% The raising primitives (called uncaught by the tests).
fc(a) -> 1.
cc(X) -> case X of a -> 1 end.
ii(X) -> if X > 0 -> 1 end.
bm(X) -> <<Y:16>> = X, Y.
tc(X) -> try X of a -> 1 catch _:_ -> nope end.

%% Catch each raise and hand the {Class, Reason} pair back to the caller.
catch_fc(X) -> try fc(X) catch C:R -> {C, R} end.
catch_cc(X) -> try cc(X) catch C:R -> {C, R} end.
catch_ii(X) -> try ii(X) catch C:R -> {C, R} end.
catch_bm(X) -> try bm(X) catch C:R -> {C, R} end.
catch_tc(X) -> try tc(X) catch C:R -> {C, R} end.

%% Just the class atom of a `cc` raise.
catch_class(X) -> try cc(X) catch C:_ -> C end.

%% Old-style `catch` over a binary-match failure.
oldcatch_bm(X) -> catch bm(X).

%% The cooked stacktrace form.
trace(X) -> try cc(X) catch C:R:S -> {C, R, S} end.
