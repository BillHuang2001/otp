%%% Oracle fixture for `bs_create_bin` with an **allocation-list** `Alloc` operand.
%%%
%%% The compiler hoists a block's pending heap need into the `bs_create_bin` that
%%% precedes it (`beam_ssa_codegen.erl:342-352`, `need_heap_terminator`), so the
%%% `Alloc` operand is written as `{alloc,[{words,N},{floats,N},{funs,N}]}` rather
%%% than a bare word count. `pack/2` needs `{words,4},{funs,1}` for the closure it
%%% builds right after the binary.
-module(bsalloc).
-export([pack/2]).

pack(X, Y) ->
    Z = <<X:8>>,
    if is_binary(Z) -> {fun() -> X end, Y};
       true -> Y
    end.
