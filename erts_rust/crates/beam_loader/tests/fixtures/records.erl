-module(records).

-export([noop/0]).

%% Native records (OTP 29 syntax). Every definition below ends up in the `Recs`
%% chunk; the module body is deliberately trivial so the only interesting payload
%% is the record table.
-record #user{id = -1, name, city}.

-record #empty{}.

%% Mixed defaults: a small-int `0` (distinct from an *absent* field), an absent
%% field, an atom default, a tuple literal, a list literal and a bignum.
-record #mixed{a = 0, b, c = undefined, d = {1, 2}, e = [a, b],
               f = 10000000000000000000000000}.

%% An exported record (`-export_record([...])`) — `is_exported => true`.
-record #shared{key, value = none}.

-export_record([shared]).

noop() -> ok.
