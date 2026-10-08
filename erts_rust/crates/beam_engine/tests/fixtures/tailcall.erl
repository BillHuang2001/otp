%% Tail-call frame-discipline oracle fixture (see ../tailcall.rs).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen" erlc +deterministic tailcall.erl`.
%%
%% ## Why this fixture exists
%%
%% `call_ext_last Arity Fun Deallocate` is "deallocate, then tail-call". In C the
%% callee runs with the current frame **intact** and the deallocation happens only
%% once the callee has returned: `emu/ops.tab:964` expands it to
%% `call_light_bif Bif Bif | deallocate_return D`, and `call_light_bif`
%% (`emu/bif_instrs.tab:262`) asserts `E == c_p->stop` across the call — the frame
%% pointer is untouched while the BIF runs. A raising BIF therefore unwinds from
%% the live frame and reports **its own** reason (`handle_error` → `next_catch`).
%%
%% The compiler is free to write a `Deallocate` operand that no longer matches the
%% live frame: `trim N` (`emu/instrs.tab:593`, `E += N`) reduces the frame, and the
%% operand of a following never-returning `call_ext_last` to an exit BIF is then
%% dead code that C never applies. `gap/1` below is exactly that shape — it is the
%% shape of `nif_res:classify/1` in the differential corpus, whose
%% `unsupported_enif_alloc_env` raise was pre-empted by a frame-discipline
%% artifact (see `erts_rust/tools/differential/CONTEXT.md`).
%%
%% ```text
%% %% tailcall:gap/1 (the `trim` + never-returning tail call shape):
%%   {label,3},
%%   {trim,1,0},                                            %% frame 1 -> 0
%%   ...
%%   {call_ext,1,{extfunc,lists,reverse,1}},
%%   {test,is_eq_exact,{f,4},[...]},
%%   {move,{integer,1},{x,0}},{deallocate,0},return,        %% the `ok` arm
%%   {label,4},
%%   {move,{atom,unsupported_gap},{x,0}},
%%   {call_ext_last,1,{extfunc,erlang,error,1},1},          %% operand 1 > frame 0
%% ```
%%
%% ## Derived with the oracle (system `erl`)
%%
%% ```text
%% tailcall:gap(6)      -> error:unsupported_gap
%% tailcall:gap(3)      -> three
%% tailcall:tailrev([1,2,3]) -> [[1,2,3]]
%% tailcall:loop(5, []) -> [1,2,3,4,5]
%% tailcall:even(10)    -> true
%% tailcall:odd(7)      -> true
%% ```
-module(tailcall).
-export([gap/1, tailrev/1, loop/2, even/1, odd/1]).

%% A frame that `trim` removes before a tail call to a raising BIF.
%%
%% `hold/1` forces `N` to stay live across a call, so the function gets one Y
%% slot; the `6` arm then `trim`s it away before the never-returning tail call.
gap(N) ->
    case hold(N) of
        ok ->
            case N of
                1 -> one;
                2 -> two;
                3 -> three;
                4 -> four;
                5 -> five;
                6 ->
                    %% The matched pattern cannot match a one-element list, so
                    %% the `_` arm (the never-returning tail call) is the one
                    %% that runs.
                    case lists:reverse([N]) of
                        [_, _] -> 1;
                        _ -> erlang:error(unsupported_gap)
                    end
            end;
        {error, Reason} ->
            reason_code(Reason)
    end.

%% Deliberately opaque to the compiler (a constant-returning local would be
%% inlined, which would drop the frame this fixture is about).
hold(_) -> ok.

reason_code(_) -> 9.

%% A **non-raising** tail call over an import, in a function that owns a frame:
%% `X` is live across two `lists:reverse/1` calls, so it lives in `y(0)` and the
%% final call is `call_ext_last 1 lists:reverse/1 1`.
tailrev(X) ->
    _ = lists:reverse([X]),
    _ = lists:reverse([X]),
    lists:reverse([X]).

%% A tail-**recursive** chain: the accumulator is carried in the arguments, so
%% the frame is `call_only`ed away every iteration (constant stack).
loop(0, Acc) -> Acc;
loop(N, Acc) -> loop(N - 1, [N | Acc]).

%% A **mutual** tail-call chain (the same constant-stack property across two
%% functions).
even(0) -> true;
even(N) -> odd(N - 1).

odd(0) -> false;
odd(N) -> even(N - 1).
