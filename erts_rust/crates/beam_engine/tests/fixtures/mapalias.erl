%% The `get_map_elements` source/destination-alias shape — regression fixture for
%% the map read family (`src/families/maps/read.rs`).
%%
%% `erlc` reuses the register that holds the source map as the FIRST pair's
%% destination whenever the map itself is dead afterwards. The instruction is
%% atomic, so every pair must still descend the map the source register held at
%% instruction entry.
%%
%% This is the shape the real world hits: `khepri_tree:gather_node_props/2`
%% (`khepri_tree.erl:268`, staged Khepri build) lowers to
%%
%%     {get_map_elements,{f,36},
%%                       {tr,{x,2},{t_map,any,any}},
%%                       {list,[{atom,payload_version},{x,2},
%%                              {atom,child_list_version},{x,3}]}}
%%
%% i.e. `Src == Dst` for the first pair. `values/1` below reproduces it from the
%% same record-with-nested-map-pattern source shape, so the fixture pins the
%% compiler's own output rather than a hand-built instruction.
%%
%% Regenerate with `erlc +deterministic` from a neutral directory (see
%% `README.md`).
-module(mapalias).
-export([values/1, missing/1]).

-record(node, {id,
               props,
               payload,
               child_nodes,
               child_list_version,
               payload_version,
               keep_while,
               expiry,
               meta}).

%% Two-pair read whose first destination is the source register:
%%     get_map_elements {f,1} {tr,{x,1},{t_map,any,any}}
%%                      [{payload_version,x,1},{child_list_version,x,2}]
values(#node{props = #{payload_version := PVersion,
                       child_list_version := CVersion},
             payload = Payload,
             child_nodes = Children}) ->
    {PVersion, CVersion, Payload, Children}.

%% One-pair read whose destination is the source register, on a key that is never
%% present: the absent key must take the `case` fail path (`error:{case_clause,_}`),
%% never a `badarg`.
missing(#node{props = #{no_such_key := Nope},
              payload = Payload,
              child_nodes = Children}) ->
    {Nope, Payload, Children}.
