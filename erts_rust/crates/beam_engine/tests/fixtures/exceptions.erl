%% Exceptions-family oracle fixture (see ../exceptions.rs).
%%
%% Compiled with the system toolchain: `erlc exceptions.erl`.
%%
%% The source sticks to opcodes the Milestone-1 interpreter + the exceptions
%% family execute: label/line/func_info/int_code_end/allocate/allocate_heap/
%% test_heap/init_yregs/deallocate/trim/move/swap/test/select_val/jump/call/
%% call_only/call_last/call_ext/call_ext_only/call_ext_last/gc_bif/bif/bif0/
%% return/badmatch/case_end/if_end/try/try_end/try_case/try_case_end/catch/
%% catch_end/raise. It deliberately avoids every put_tuple2/get_tuple_element/
%% put_list/get_list/map/fun/float/bitstring opcode (still stubs), so catch
%% clauses match tuple elements only with `_`.
-module(exceptions).
-export([case_clause/1, badmatch/1, if_clause/1, try_clause/1, oldcatch/1,
         oldcatch_match/1, guard_badarith/1, nested/1, deep/1,
         case_clause_raw/1, badmatch_raw/1, if_clause_raw/1, try_clause_raw/1]).

case_clause(X) -> try case X of 1 -> one end catch error:{case_clause,_} -> caught_case_clause end.

badmatch(X) -> try ok = X, matched catch error:{badmatch,_} -> caught_badmatch end.

if_clause(X) -> try if X > 0 -> positive end catch error:if_clause -> caught_if_clause end.

try_clause(X) -> try inner_of(X) catch error:{try_clause,_} -> caught_try_clause end.

inner_of(X) -> try X of 1 -> one catch throw:_ -> inner_throw end.

%% Old-style catch: the raw caught value escapes to the caller.
oldcatch(X) -> catch (case X of 1 -> one end).

%% The same, inspected in-module (this also interns 'EXIT' so the tuple the
%% handler builds is representable).
oldcatch_match(X) -> case catch (case X of 1 -> one end) of {'EXIT',_} -> caught_exit; Value -> Value end.

%% A guard BIF that returns an Err makes the guard fail (oracle: `{case_clause,N}`).
guard_badarith(X) -> try case X of N when N / 0 > 0 -> positive end catch error:{case_clause,_} -> caught_case_clause end.

nested(X) -> try (try case X of 1 -> one end catch throw:_ -> inner_throw end) catch error:{case_clause,_} -> outer_case_clause end.

deep(X) -> try deep_inner(X) catch error:{case_clause,_} -> deep_case_clause end.

deep_inner(X) -> try case X of 1 -> one end catch throw:_ -> inner_throw end.

case_clause_raw(X) -> case X of 1 -> one end.

badmatch_raw(X) -> ok = X.

if_clause_raw(X) -> if X > 0 -> positive end.

try_clause_raw(X) -> try X of 1 -> one catch throw:_ -> inner_throw end.
