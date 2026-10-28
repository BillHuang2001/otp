# HANDOFF — the `tools/differential` case that pins accepted-socket ownership

**Owner of the edit:** the node that owns `erts_rust/tools/differential/` (sibling, read-only here).
`erts_rust/crates/beam_rt` fixed the *behaviour*; this file carries the *gate case* so it can be applied verbatim.

## Why

`crates/beam_rt/src/port/completion.rs` now adopts an accepted descriptor with the accept's **issuer** as the new
port's connected process (the C's `driver_caller`, `erts/emulator/drivers/common/inet_drv.c:12233` → `tcp_inet_copy`
`:11834`), not the listener's owner. Two consequences are user-visible and only this shape distinguishes them:
`erlang:port_info(S, connected)` inside the acceptor names the acceptor, and `inet:tcp_controlling_process/2` then
succeeds and moves the owner to a third process.

No existing corpus case covers it: no case id contains `controlling`, `sendfile` or `advise`, and the `driver*` /
`inet_*` rows only pin the port's shape, never issuer-versus-owner for an accepted socket.

## The case (one file: `erts_rust/tools/differential/src/corpus/inet_control.rs`)

Append to `group()`'s `vec![…]` (the last case is `… , "ok {{error,timeout},ok}", ),`):

```rust
        // --- accept ownership: the accepting process, not the listener's owner ----------------
        case(
            "inet_control/tcp_accept_ownership_follows_the_acceptor",
            "a connection accepted by a process that does not own the listener is owned by the \
             accepting process — erlang:port_info(S,connected) names it, never the listener's \
             owner — and inet:tcp_controlling_process/2 then hands the socket on to a third \
             process, whose port_info(S,connected) names it in turn",
            &[Contract::DriverAbi, Contract::OtpApps],
            "begin {ok, L} = gen_tcp:listen(0, [{ip, {127, 0, 0, 1}}, binary, \
                                               {active, false}]), \
             {ok, Port} = inet:port(L), \
             Parent = self(), \
             Acceptor = spawn(fun() -> \
                 {ok, S} = gen_tcp:accept(L), \
                 Self = self(), \
                 At = case erlang:port_info(S, connected) of \
                          {connected, Who} -> {connected, Who =:= Self}; \
                          Other -> Other \
                      end, \
                 Next = spawn(fun() -> \
                     receive {took, S2, From} -> \
                         Obs = case erlang:port_info(S2, connected) of \
                                   {connected, Who2} -> {connected, Who2 =:= self()}; \
                                   Other2 -> Other2 \
                               end, \
                         erlang:send(From, {moved, Obs}), \
                         (catch gen_tcp:close(S2)) \
                     after 2000 -> ok end \
                 end), \
                 Moved = gen_tcp:controlling_process(S, Next), \
                 erlang:send(Next, {took, S, self()}), \
                 Got = receive {moved, Obs2} -> Obs2 after 2000 -> timeout end, \
                 erlang:send(Parent, {accepted, At, Moved, Got}) \
             end), \
             {ok, Cl} = gen_tcp:connect({127, 0, 0, 1}, Port, [binary, {active, false}]), \
             Res = receive {accepted, At2, Moved2, Got2} -> {At2, Moved2, Got2} \
                   after 5000 -> timeout end, \
             ok = gen_tcp:close(Cl), \
             ok = gen_tcp:close(L), \
             {is_pid(Acceptor), Res} end",
            "ok {true,{{connected,true},ok,{connected,true}}}",
        ),
```

(`Contract::DriverAbi` alone is also accepted if the neighbouring rows prefer a single contract.)

Then bump the group's size pin (same file, in its guard test):

```rust
        assert_eq!(cases.len(), 18, "the group's size is pinned by this test");
```
→ `19`.

Optional doc bullet next to the `passive_recv_on_an_empty_socket_times_out` note:

```rust
//! * **`tcp_accept_ownership_follows_the_acceptor`** — the accepted connection belongs to the process
//!   that called `gen_tcp:accept/1`, not to the listener's owner: the acceptor and, after
//!   `inet:tcp_controlling_process/2`, the third process each see `port_info(S, connected)` name
//!   *themselves* (both pids reduced to `=:= self()`), so the golden carries no volatile pid.
```

The guards this row must satisfy (verified against the text above): only `inet_control/` ids; non-empty
description; a single-line golden with exit `0`; no `port_command`/`prim_inet`/`os:cmd`; no `gen_tcp:recv(`;
no bare `V`/`C`/`R`/`X` token (the probe wrapper's names — hence `Next`/`Self`/`Who`); every `receive` bounded by
`after`; and one `gen_tcp:close(L)` present.

## The golden is measured, not predicted

The expression above was run unchanged under both runtimes on this tree (`beam_rt` at `c800645985` + the merges):

```
oracle (OTP 29.1.2 / erts-17.1.1)  ok {true,{{connected,true},ok,{connected,true}}}   exit 0
rust   (erts_rust/target/debug/beam_rt)  ok {true,{{connected,true},ok,{connected,true}}}   exit 0
```

Three rust repeats and two oracle repeats were byte-identical, and no socket was left listening. Before the
ownership fix the rust line was `{connected,false}` / `{error,not_owner}` (a `mismatch`, not a gap), which is why
this case must ship **with or after** the `beam_rt` fix and never before it.

## Verification after applying

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p differential -p beam_rt
erts_rust/target/debug/differential run inet_control/tcp_accept_ownership_follows_the_acceptor
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p differential --lib   # the group's guards
```

Expected: `[AGREE]`, `total=1 agree=1 mismatch=0`; the guard suite green with the size pin at 19.
`news`/`list` pick the id up automatically — no `mod.rs`, allowlist or golden file to touch.
