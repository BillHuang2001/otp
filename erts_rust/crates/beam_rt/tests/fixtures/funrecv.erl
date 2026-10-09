%% The **fun-bodied** process entry: "a fun that crosses a module boundary into a
%% blocking `receive`" (the corner the `ProcessEntry::Fun` runner arm used to fold into
%% the typed `blocked()` capability gap instead of parking the process).
%%
%% `erlang:spawn/1` (and the fun head of `spawn_link/1`/`spawn_opt/2`) makes a process
%% whose entry point is a **fun value**. Applying that fun is not an ordinary call: a
%% `.beam` local (`make_fun3`) closure has to be entered at *its own* lambda label in the
%% module that defines it — and on the *process's* machine — for a `receive` inside it to
%% suspend the process and be resumed by a later message. `tests/fun_entry_receive.rs` is
%% the gate; the shapes here are:
%%
%% * `run_cross/0`  — the fun body calls `funrecvw:wait_ready/1`, a **different module**,
%%   which blocks in `receive` (the exact shape `proc_lib:init_p/5`'s
%%   `spawn_opt(fun () -> init_p(M, F, A) end)` has, one `call_ext` away from the fun);
%% * `run_inline/0` — the waiting `receive` is **inline in the fun body**;
%% * `mkchild/0`    — the 0-arity local fun the API-level test spawns directly
%%   (`Runtime::spawn_fun_process`) so it can watch the process park before it is woken;
%% * `wake/1`       — the export the same test uses to wake that child.
%%
%% Every entry point answers the bare integer `42` (or `timeout`/`{down, Reason}` when the
%% child never got that far), so both runtimes print the same line.
-module(funrecv).

-export([run_cross/0, run_inline/0, mkchild/0, wake/1]).

%% The fun body calls a **cross-module** function that blocks in `receive`. The child
%% announces itself before it blocks, so the parent only sends the wake-up after the child
%% has reached its `receive` — which is what makes the answer proof of a real
%% suspend-and-resume rather than of a message that was already lying in the mailbox.
run_cross() ->
    Parent = self(),
    {Pid, Ref} = erlang:spawn_monitor(fun() -> funrecvw:wait_ready(Parent) end),
    receive {ready, Pid} -> ok end,
    Pid ! {go, Parent},
    wait_ack(Pid, Ref).

%% The same shape with the waiting `receive` **inline in the fun body** (no module
%% boundary): the fun's own label entry must run it on the process's machine too.
run_inline() ->
    Parent = self(),
    {Pid, Ref} = erlang:spawn_monitor(fun() ->
        Parent ! {ready, self()},
        receive {go, Parent} -> Parent ! {ack, self(), 42} end
    end),
    receive {ready, Pid} -> ok end,
    Pid ! {go, Parent},
    wait_ack(Pid, Ref).

%% The parent's wait for the child's ack — an ordinary module-export process's receive
%% (this shape already worked before the fun entry could park).
wait_ack(Pid, Ref) ->
    receive
        {ack, Pid, R} -> erlang:demonitor(Ref, [flush]), R;
        {'DOWN', Ref, process, Pid, Reason} -> {down, Reason}
    after 5000 -> timeout
    end.

%% The 0-arity local fun of the API-level test's child: no free variables, so it isolates
%% the `free ++ args` arity the resolver derives. Its body parks in `funrecvw:wait_go/0`.
mkchild() ->
    fun() -> funrecvw:wait_go() end.

%% Wakes a child parked in `funrecvw:wait_go/0` (the API-level test's second process).
wake(Pid) ->
    Pid ! {go, 42}.
