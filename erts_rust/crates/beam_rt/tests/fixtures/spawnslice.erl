%% Fixture for `tests/spawn_from_slice.rs` (compiled at test time — never committed as a
%% `.beam`).
%%
%% `run/0` is entered by `Runtime::run_scheduled`, i.e. as an ordinary **scheduled process**:
%% every `spawn*` below therefore happens *inside a scheduler slice*. Each child is expected
%% to run and to reach its parent, so `run/0` returns one element per spawn form —
%% `true` when the child really ran and reported back, a named failure atom otherwise:
%%
%%   * a fun-bodied `spawn/1`;
%%   * an export-bodied `spawn/3`;
%%   * `spawn_link/1` (the link target);
%%   * `spawn_opt/2` with `monitor` (the child's `{'DOWN', _, process, _, normal}`);
%%   * a **nested** spawn: a child that spawns its own child from *its* slice.
%%
%% The same module runs unchanged under the system `erl` oracle, so the two runtimes are
%% compared on the same program.
-module(spawnslice).

-export([run/0, tracked/0, rejected/0, export_child/1, nested_child/1]).

%% Entered as a scheduled process; see the module comment.
run() ->
    Parent = self(),
    Fun = spawn(fun() -> Parent ! {fun_child, self()} end),
    Export = spawn(?MODULE, export_child, [Parent]),
    Link = spawn_link(fun() -> Parent ! {link_child, self()} end),
    {Mon, Ref} = spawn_opt(fun() -> Parent ! {mon_child, self()} end, [monitor]),
    _Nested = spawn(fun() -> spawn(?MODULE, nested_child, [Parent]) end),
    [
        waited(fun_child, Fun),
        waited(export_child, Export),
        waited(link_child, Link),
        waited(mon_child, Mon),
        waited(nested_child, ignored),
        down(Ref, Mon)
    ].

%% The export-bodied child: reports its own pid to the parent.
export_child(Parent) ->
    Parent ! {export_child, self()}.

%% The child a *child* spawns (the nested spawn path).
nested_child(Parent) ->
    Parent ! {nested_child, self()}.

%% The child must be visible in the **pass** process table the parent's slice runs against
%% (not the runtime's own, which is an empty placeholder for the duration of the pass), and
%% it must really run: `{true, true}`.
tracked() ->
    Parent = self(),
    Child = spawn(fun() -> Parent ! {tracked, self()} end),
    Listed = lists:member(Child, processes()),
    receive
        {tracked, P} -> {Listed, P =:= Child}
    after 5000 -> {Listed, timeout}
    end.

%% A rejected `spawn_opt` option list must not leave a child behind: `{badarg, true}`.
rejected() ->
    Before = length(processes()),
    Outcome = try spawn_opt(fun() -> ok end, [bogus])
              catch error:badarg -> badarg
              end,
    After = length(processes()),
    {Outcome, Before =:= After}.
%% Whether the `{Tag, Pid}` message arrived: `true` when the reported pid is the one the
%% parent holds (`ignored` for a pid the parent cannot know yet — the grandchild's).
waited(Tag, Pid) ->
    receive
        {Tag, P} -> P =:= Pid orelse Pid =:= ignored
    after 5000 -> {Tag, timeout}
    end.

%% Whether the monitored child's `{'DOWN', …}` arrived, with the exit reason `normal`.
down(Ref, Pid) ->
    receive
        {'DOWN', R, process, P, normal} when R =:= Ref, P =:= Pid -> true
    after 5000 -> down_timeout
    end.
