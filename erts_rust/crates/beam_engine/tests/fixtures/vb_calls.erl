%% Call/frame-control variant-proof fixture (see ../real_beam_variant_proofs/calls.rs).
%%
%% Compiled with the system toolchain from a *neutral* directory:
%% `env -C "$TMPDIR/fixgen/..." erlc +deterministic vb_calls.erl`.
%%
%% ## Why this fixture exists
%%
%% Each export forces the OTP compiler (OTP 29 / `erts-17.1`) to emit exactly one
%% of the call/frame-control instructions, so a *real* `.beam` — not a hand-built
%% `beam_loader::Module` — proves the engine executes it:
%%
%% | export | compiled shape |
%% |---|---|
%% | `call_nontail/1` | `allocate 0,1` · `call vb_calls:sub/1` · `gc_bif '*'` · `deallocate 0` · `return` (fused to `DeallocateReturn`) |
%% | `call_only/1` | frameless body, so the tail call is `call_only vb_calls:sub/1` |
%% | `call_last/1` | one Y slot, so the tail call is `call_last 1 vb_calls:sub/1, 1` |
%% | `ext_nontail/1` | `call_ext 1 lists:reverse/1` · `put_tuple2` · `deallocate 1` · `return` |
%% | `ext_only/1` | frameless body, so the tail call is `call_ext_only 1 lists:reverse/1` |
%% | `ext_last/1` | one Y slot, so the tail call is `call_ext_last 1 lists:reverse/1, 1` |
%% | `yregs/1` | `allocate_heap 2,2,1` · `init_yregs [y0,y1]` (a `try` whose handler keeps `Y`) |
%% | `clause/1` | a lone head guard with **no stack frame**, so a failed guard jumps to the `func_info` label (`{test,is_integer,{f,<func-info-label>},…}`) |
%% | `clause_catch/1` | the caller that catches that `function_clause` |
%% | `node_bif/0` | `bif0 node {x,0}` (a zero-argument BIF with no fail label) |
%% | `trimming/1` | `allocate 1,1` · `call` · `select_val` · **`trim 1,0`** · `call_ext` — one Y slot dies in the arm, so `beam_trim` shrinks the frame |
%% | `moving/1` | kept for completeness (the optimizer folds it away) |
%%
%% ## Derived with the oracle (system `erl`)
%%
%% ```text
%% vb_calls:call_nontail(3)  -> 8
%% vb_calls:call_only(4)     -> 5
%% vb_calls:call_last(1)     -> 3
%% vb_calls:ext_nontail(2)   -> {[2],2}
%% vb_calls:ext_only([1,2,3])-> [3,2,1]
%% vb_calls:ext_last([5])    -> [[5]]
%% vb_calls:yregs(7)         -> {ok,[[7]]}
%% vb_calls:clause(5)        -> 10
%% vb_calls:clause_catch(a)  -> {error,function_clause}
%% vb_calls:node_bif()       -> nonode@nohost
%% vb_calls:trimming(2)      -> {two,[5]}
%% vb_calls:trimming(1)      -> one
%% ```
-module(vb_calls).
-export([call_nontail/1, call_only/1, call_last/1,
         ext_nontail/1, ext_only/1, ext_last/1,
         yregs/1, clause/1, clause_catch/1, node_bif/0,
         trimming/1, moving/1]).

%% A local callee the compiler will not inline: it is the target of every
%% `call`/`call_only`/`call_last` shape below.
sub(X) -> X + 1.

%% `call` (non-tail) + `deallocate 0`/`return` (fused into `DeallocateReturn`):
%% the result of `sub/1` is consumed by a `gc_bif`, so the call cannot be a tail
%% call.
call_nontail(X) ->
    Y = sub(X),
    Y * 2.

%% `call_only`: no Y registers and no catch frame, so the tail call needs no
%% deallocation.
call_only(X) -> sub(X).

%% `call_last`: one Y slot (the second `sub/1` argument is live across the
%% middle call), so the tail call deallocates it.
call_last(X) ->
    Y = sub(X),
    _ = sub(Y),
    sub(Y).

%% `call_ext` (non-tail) + `deallocate 1`/`return`.
ext_nontail(X) ->
    Y = lists:reverse([X]),
    {Y, X}.

%% `call_ext_only`: frameless, so the tail call over the import deallocates
%% nothing.
ext_only(X) -> lists:reverse(X).

%% `call_ext_last`: one Y slot, so the tail call over the import deallocates it.
ext_last(X) ->
    Y = lists:reverse([X]),
    _ = lists:reverse(Y),
    lists:reverse(Y).

%% `init_yregs`: the handler needs `Y`, so the frame's Y slots are initialized
%% before the `try` body can leave them holding stale words.
yregs(X) ->
    Y = lists:reverse([X]),
    try
        Z = lists:reverse([Y]),
        {ok, Z}
    catch
        _:_ -> {error, Y}
    end.

%% `func_info`: a single guarded clause with no stack frame. A failed guard is a
%% `function_clause` the compiler lowers to a jump to the **`func_info` label**
%% (`beam_ssa_codegen:cg_match_fail/3`'s `fc_label`), which is the only way real
%% compiler output reaches that instruction.
clause(X) when is_integer(X) -> X * 2.

%% The caller that observes the `func_info` raise: the reason term is the bare
%% atom `function_clause`.
clause_catch(X) ->
    try clause(X) catch error:R -> {error, R} end.

%% `bif0`: a zero-argument BIF with no fail label, encoded as
%% `bif0 {extfunc,erlang,node,0} {x,0}`.
node_bif() -> node().

%% Deliberately opaque to the compiler (a constant-returning local would be
%% inlined, and `N` would stop being live across the call).
hold(_) -> ok.

%% `trim`: `N` lives in `y(0)` across the `hold/1` call; in the `2` arm it is
%% dead, so `beam_trim` shrinks the frame to 0 (`trim 1,0`) before the `call_ext`
%% that follows. `hold/1` returns `ok`, so the compiler also proves the outer
%% `case`'s `ok` pattern and leaves the `select_val` on `N`.
trimming(N) ->
    case hold(N) of
        ok ->
            case N of
                1 -> one;
                2 ->
                    Y = lists:reverse([5]),
                    {two, Y};
                _ ->
                    Y = lists:reverse([N]),
                    {other, Y}
            end;
        {error, Reason} -> {error, Reason}
    end.

%% Kept as a readable `move` shape; the optimizer folds the two aliases into the
%% returned tuple, so this export is documentation rather than a `move` source.
moving(X) ->
    Y = X,
    Z = Y,
    {Y, Z}.
