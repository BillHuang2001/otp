-module(raise_structured).
-export([badkey/0, badmap/0]).

badkey() -> maps:get(nokey, #{}).
badmap() -> map_get(a, not_a_map).
