# PARENT_WIRING — wiring `port::ops::active_input` and `port::ops::inet_opts` into `src/port.rs`

**Owner of this file:** the `src/port` node. **Target of the patch:** the *parent-owned*
`erts_rust/crates/beam_rt/src/port.rs`. This node may not edit `.rs` files, so this document is a
**ready-to-apply patch as text** plus everything the parent needs to verify it.

**Status: the core patch was applied to a throwaway copy of the tree, compiled (`-p beam_rt --lib`
and `--all-targets`, both **zero warnings / zero errors**), and the two target suites were run to
green (`25/25` and `4/4`) once the documented *test-stand-in* calls are neutralized.** The only
required companion changes are in `.rs` test files that are outside this node's write scope; see
§6. Nothing in the patch is guessed — every hunk is quoted verbatim from the current file.

Both diffs below were checked with `git apply --check` against a pristine copy of the current
`src/port.rs`: **`core.diff` applies clean; `o1.diff` applies clean on top.**

---

## 1. What the two modules provide (already finished, green, in `src/port/`)

| module | declares itself via | public surface used here |
|---|---|---|
| `src/port/active_input.rs` | `#[path]` from `ops.rs:89-90` (`pub(crate) mod active_input;` → `port::ops::active_input`) | `PortRegistry::drain_active_inputs(&mut self) -> Vec<ActiveInputPacket>`; `ActiveInputPacket { port, owner, data_mode, kind, event }`; `RuntimeContext::{active_input_message, active_data_message, active_passive_message, socket_atom}` |
| `src/port/inet_opts.rs` | `#[path]` from `ops.rs:100-101` (`pub(crate) mod inet_opts;` → `port::ops::inet_opts`) | `PortRegistry::note_inet_setopts(&mut self, id, arg, ok) -> bool`; `is_setopts_control(&str) -> bool`; `setopts_succeeded(&[u8]) -> bool`; `PortRegistry::data_mode(id, fallback)` |

Because they are declared **inside `ops.rs`**, the paths are `ops::active_input::…` and
`ops::inet_opts::…` from `port.rs`. **No `mod` line changes** (the `mod` block at `port.rs:171-177`
stays exactly as it is).

Baseline (current tree, unpatched) — see §7 for the exact commands:

```
warning: `beam_rt` (lib) generated 33 warnings        # 23 in inet_opts.rs + 10 in active_input.rs, ALL dead-code
test result: ok. 25 passed; 0 failed; … 1540 filtered out     # -- port::ops::active_input
test result: ok.  4 passed; 0 failed; … 1561 filtered out     # -- port::completion::accept_inherit
```

All 33 warnings are the not-yet-wired dead-code class (`INET_CONTROL_MAGIC`, `RecordBytes`,
`record`, `note_inet_setopts`, `is_setopts_control`, `walk`, … ; `ActivePortKind`,
`ActiveInputPacket`, `data_mode`, `drain_active_inputs`, `decode_packed_address`,
`active_input_message`, …). **There is no other warning class in the crate's library build.** They all
vanish with the patch (§7).

---

## 2. The core patch — 11 unified hunks

`diff --git a/erts_rust/crates/beam_rt/src/port.rs b/erts_rust/crates/beam_rt/src/port.rs`

Current-side line numbers are noted per hunk (§2.1) because they drift; **the hunk context is what
matters**. The whole block below is the literal output of `diff -u` and applies with
`git apply`.

```diff
--- a/erts_rust/crates/beam_rt/src/port.rs
+++ b/erts_rust/crates/beam_rt/src/port.rs
@@ -828,6 +828,18 @@
         /// The drained completion, its recipient resolved.
         packet: completion::CompletionPacket,
     },
+    /// An **`{active, …}` input delivery**: the `{tcp, S, Data}` / `{udp, S, IP, Port, Data}` /
+    /// `{tcp_passive, S}` / `{udp_passive, S}` message a socket port's driver hands up while the
+    /// port is in an active mode (see [`ops::active_input`]).
+    ///
+    /// Like [`PortEvent::Completion`] it carries its **recipient already resolved**
+    /// ([`ops::active_input::ActiveInputPacket::owner`], together with the stream mode and the
+    /// socket kind bound at drain time), so nothing is read from a port table the delivering
+    /// route may no longer hold — which is why [`PortEvent::port`] answers `None` for it.
+    ActiveInput {
+        /// The drained delivery, its recipient resolved.
+        packet: ops::active_input::ActiveInputPacket,
+    },
     /// A **handled signal** arrived (`os:set_signal(Sig, handle)`) — the delivery half of the
     /// A.4/A.8 signal surface, drained from the self-pipe [`watch::SignalWatch`] registers.
     ///
@@ -863,8 +875,9 @@
 }
 
 impl PortEvent {
-    /// The port this event is **about**, or `None` for the two node-global / owner-keyed kinds
-    /// that no port carries (a handled signal and a `tty_select/2` report).
+    /// The port this event is **about**, or `None` for the events that already carry their
+    /// recipient resolved (a completion, an `{active, …}` delivery) and for the two node-global
+    /// kinds that no port carries (a handled signal and a `tty_select/2` report).
     ///
     /// Used where an event has to be checked against the registry's live ports before it is
     /// reported — [`PortRegistry::drain_pending_events`], which drops a retained event whose port
@@ -882,6 +895,11 @@
             // ones its owner is still owed. It carries its own owner, so a recycled port number
             // cannot mis-deliver it either.
             PortEvent::Completion { .. } => None,
+            // An `{active, …}` delivery is bound to its owner, mode and kind at drain time (see
+            // [`ops::active_input`]), so — like a completion — filtering it against
+            // [`PortRegistry::meta`] would drop exactly the deliveries a closing port's owner is
+            // still owed, and a recycled port number could not mis-deliver it either.
+            PortEvent::ActiveInput { .. } => None,
             PortEvent::Signal { .. } | PortEvent::Select { .. } => None,
         }
     }
@@ -1317,6 +1335,12 @@
             // reason to fail the command.
             let completions = self.drain_io_completions(None);
             self.pending_events.extend(completions);
+            // The same `output` callback is where a socket driver stages an `{active, …}`
+            // delivery, so that channel is drained beside the completions and held as pending
+            // work until a pump reports it (see [`ops::active_input`]).
+            let active = self.drain_active_inputs();
+            self.pending_events
+                .extend(active.into_iter().map(|packet| PortEvent::ActiveInput { packet }));
             written?;
             return Ok(());
         }
@@ -1348,6 +1372,11 @@
         // command is exactly where a socket driver stages `gen_udp:send`'s `{inet_reply, …}`.
         let completions = self.drain_io_completions(None);
         self.pending_events.extend(completions);
+        // Same rule as the driver-`output` branch above: the round that carried this command is
+        // where a socket driver stages an `{active, …}` delivery too.
+        let active = self.drain_active_inputs();
+        self.pending_events
+            .extend(active.into_iter().map(|packet| PortEvent::ActiveInput { packet }));
         Ok(())
     }
 
@@ -1416,6 +1445,21 @@
         // it staged.
         let completions = self.drain_io_completions(None);
         self.pending_events.extend(completions);
+        // The same callback is where a socket driver stages an `{active, …}` delivery (a
+        // `setopts` that reads inline does), so that channel is drained here too.
+        let active = self.drain_active_inputs();
+        self.pending_events
+            .extend(active.into_iter().map(|packet| PortEvent::ActiveInput { packet }));
+        // A `setopts` control op is also where a socket port's option block becomes part of its
+        // mirror: record the argument the driver **accepted** — its reply's leading
+        // `INET_REP_OK` tag (`inet_drv.c:870`) — so an accepted socket can inherit it (see
+        // [`ops::inet_opts`]). The reply is read, never consumed: a refused `setopts` is the
+        // `Ok([0]einval)` reply and records nothing, and a failing call keeps its own path.
+        if ops::inet_opts::is_setopts_control(op) {
+            if let Ok(bytes) = &reply {
+                let _ = self.note_inet_setopts(id, arg, ops::inet_opts::setopts_succeeded(bytes));
+            }
+        }
         reply
     }
 
@@ -1674,6 +1718,13 @@
         // snapshot, because the port's metadata is already gone — is what keeps that reply.
         let completions = self.drain_io_completions(snapshot);
         self.pending_events.extend(completions);
+        // The teardown's round is a drive point for the `{active, …}` channel as well. A delivery
+        // staged for **this** port has no metadata any more, so the drain drops it — the
+        // completion channel's rule — while a delivery staged for another live port is held like
+        // any other pending work (see [`ops::active_input`]).
+        let active = self.drain_active_inputs();
+        self.pending_events
+            .extend(active.into_iter().map(|packet| PortEvent::ActiveInput { packet }));
         // `and` returns the teardown's error when it failed, and the notification's
         // otherwise — so a failure on either path reaches the caller.
         closed.and(notified)
@@ -1759,6 +1810,13 @@
             // is drained here — otherwise a completion awaiting the last port's teardown would
             // be stranded for ever.
             events.extend(self.drain_io_completions(None));
+            // That round minted the last port's close report, which is also a drive point for the
+            // `{active, …}` channel (see [`ops::active_input`]).
+            events.extend(
+                self.drain_active_inputs()
+                    .into_iter()
+                    .map(|packet| PortEvent::ActiveInput { packet }),
+            );
             return Ok(events);
         }
         let deadline = Instant::now() + timeout;
@@ -1825,6 +1883,13 @@
             // deferring the drain until something else looked non-empty would starve a caller
             // blocked on that very message (see [`completion`]).
             events.extend(self.drain_io_completions(None));
+            // The `{active, …}` channel is drained here too: a driver stages a delivery from the
+            // same `ready_input` callback a completion comes from (see [`ops::active_input`]).
+            events.extend(
+                self.drain_active_inputs()
+                    .into_iter()
+                    .map(|packet| PortEvent::ActiveInput { packet }),
+            );
             // The **line channel** is drained on *every* round, immediately after the tasks:
             // `take_line_notifications` empties every line-mode port's staged frames, so
             // deferring it until something else looked non-empty would lose them (see
@@ -3447,6 +3512,15 @@
                         packet.port.to_eterm(),
                         ctx.completion_message(packet)?,
                     ),
+                    // An `{active, …}` input delivery: the port is the sender (`S`) and the
+                    // recipient is the owner the drain already resolved, so a receiver blocked in
+                    // `receive` is woken by the ordinary `deliver_in_table` call below (see
+                    // [`ops::active_input`]).
+                    PortEvent::ActiveInput { packet } => (
+                        packet.owner,
+                        packet.port.to_eterm(),
+                        ctx.active_input_message(packet)?,
+                    ),
                     // A handled signal goes to the `erl_signal_server` process by name, with the
                     // emulator (`system`) as the sender. An unregistered name resolves to no pid,
                     // in which case there is nobody to notify and the signal is dropped.
@@ -3581,6 +3655,15 @@
                     }
                     messages.push(self.completion_message(&packet)?);
                 }
+                // An `{active, …}` input delivery. Same rule as the completion arm above: the
+                // evaluator observes only its **own** mailbox, so one addressed to another
+                // process is left to the scheduler route.
+                PortEvent::ActiveInput { packet } => {
+                    if packet.owner != self.pid() {
+                        continue;
+                    }
+                    messages.push(self.active_input_message(&packet)?);
+                }
                 PortEvent::Signal { .. } => continue,
                 // A ready `tty_select/2` interest belongs in the **owning reader's** mailbox, which
                 // the evaluator is not: it can only observe its own mailbox, so — like the signal
```

### 2.1 Hunk table (location · current code · replacement · rationale)

| # | Location (current line) | Current code (abridged) | Replacement | Rationale |
|---|---|---|---|---|
| H1 | `PortEvent` enum, after `Completion` (`~828-833`) | `Completion { packet: completion::CompletionPacket },` | adds `ActiveInput { packet: ops::active_input::ActiveInputPacket },` | The provider needs an event kind to carry a drained delivery through `pending_events`/`pump`. |
| H2 | `impl PortEvent` doc (`~863-866`) | "or `None` for the two node-global / owner-keyed kinds" | "or `None` for the events that already carry their recipient resolved (a completion, an `{active, …}` delivery) and for the two node-global kinds…" | Keeps the `port()` doc truthful for the new variant. |
| H3 | `PortEvent::port()` (`~882-885`) | `Completion { .. } => None,` then `Signal \| Select => None` | inserts `ActiveInput { .. } => None,` before `Signal \| Select` | **Load-bearing:** a delivery must survive a close (its port's metadata is gone) exactly like a completion; returning `None` is what stops `close`'s purge and `drain_pending_events` dropping it. |
| H4 | `write_command`, driver-`output` branch (`~1317`) | `let completions = self.drain_io_completions(None); self.pending_events.extend(completions);` | adds `let active = self.drain_active_inputs(); self.pending_events.extend(active.into_iter().map(…ActiveInput…));` | The `.so` driver's `output` callback stages active input; hold it as pending work. |
| H5 | `write_command`, OUTPUT-task branch (`~1348`) | same completion drain before `Ok(())` | same active drain | A built-in driver's port-task round is the same drive point. |
| H6 | `port_control` (`~1416`) | completion drain, then `reply` | adds the active drain **and** the `note_inet_setopts` hook (see §5) | A `setopts` that reads inline stages a delivery **and** is where the option mirror is fed. |
| H7 | `close` (`~1674`) | `let completions = self.drain_io_completions(snapshot); self.pending_events.extend(completions);` | adds the active drain | The teardown round is a drive point; a delivery for *this* port is dropped by the drain, another port's is retained. |
| H8 | `pump`, "nothing live to poll" early return (`~1759`) | `events.extend(self.drain_io_completions(None)); return Ok(events);` | adds `events.extend(self.drain_active_inputs().into_iter().map(…));` before the `return` | That round minted the last close report — the last drive point before the caller is told nothing is pending. |
| H9 | `pump`, every round (`~1825`) | `events.extend(self.drain_io_completions(None));` | adds the active drain right after | The ordinary per-round drive point; both routes consume `pump`'s returned events inline. |
| H10 | `Runtime::deliver_port_messages` match (`~3447`) | `Completion { packet } => (packet.owner, packet.port.to_eterm(), ctx.completion_message(packet)?),` | adds `ActiveInput { packet } => (packet.owner, packet.port.to_eterm(), ctx.active_input_message(packet)?),` | The scheduled route: recipient = `packet.owner`, sender = the port; the existing `deliver_in_table` below wakes a blocked receiver. Note the match scrutinee is `&event`, so `packet` binds by reference — no `&`. |
| H11 | `RuntimeContext::poll_port_messages` match (`~3581`) | `Completion { packet } => { if packet.owner != self.pid() { continue; } … }` | adds the same guard + `messages.push(self.active_input_message(&packet)?)` | The `-eval` route observes only its own mailbox; `packet` binds **by value** here (the scrutinee is an owned `event`), hence the `&packet`. |

**No `mod` change** (see §1) is needed.

### 2.2 Why `has_nothing_to_poll()` stays truthful (requirement #4) — **no extra arm needed**

`PortRegistry::has_nothing_to_poll` (`port.rs:1604-1610`) is:

```rust
self.meta.is_empty()
    && self.pending_events.is_empty()
    && self.pending_port_signals.is_empty()
    && !self.tty.is_armed()
    && !self.watch.has_pending_signal()
```

A drained active delivery rides `pending_events` (H4–H9), so `pending_events.is_empty()` is `false`
until it is reported — **exactly the `Completion` precedent**. And because H3 makes
`PortEvent::port()` return `None`, `close`'s purge (`event.port() != Some(id)`) never removes it, so
a delivery for a *closed* port is still reported (to its resolved owner) rather than lost. A separate
lane is therefore **not** needed and none is provided.

---

## 3. Optional addendum O1 — resolve the **effective** `{mode, …}` before rendering (3 hunks)

**This is a genuine oracle-fidelity gap, not a bug in the wiring.** `ActiveInputPacket::data_mode`
is bound at drain time from `PortMeta::data_mode()`, which returns the port's **open-time** stream.
`prim_inet:open/6` always opens the socket port `[binary]`
(`erts/preloaded/src/prim_inet.erl:99-137`), so a `{mode, list}` socket's packet still says
`Binary`. The **effective** mode lives only in the inet mirror and must be read through
`PortRegistry::data_mode(port, fallback)` (see `src/port/inet_opts.rs:356-361`, whose doc calls this
out explicitly). `src/port/active_input_e2e_tests.rs:441-466` (the `resolved()`/`message()` helpers)
and its module docs (`:49-62`) state that *the provider's delivery arm must perform that resolution*.
Without O1, an active delivery on a `{mode,list}` socket renders a **binary** where the oracle renders
a **char list**.

O1 resolves at the **delivery arms** (not at the staging sites), which keeps `packet.data_mode ==
Binary` at the drain boundary — exactly what the e2e suite asserts — and closes the render gap.

```diff
diff --git a/erts_rust/crates/beam_rt/src/port.rs b/erts_rust/crates/beam_rt/src/port.rs
--- a/erts_rust/crates/beam_rt/src/port.rs
+++ b/erts_rust/crates/beam_rt/src/port.rs
@@ -3452,6 +3452,19 @@
         let signal_server = self.code_server.atoms().lookup("erl_signal_server");
         let mut delivered = 0;
         for event in events {
+            // An `{active, …}` delivery is rendered in the port's **effective** data mode — the
+            // `{mode, …}` record its driver accepted (`inet_opts`), with the open-time stream as
+            // the fallback — so a `{mode,list}` socket renders a char list as the oracle's does.
+            // Resolved here, before `RuntimeContext` borrows `self`.
+            let active_mode = match &event {
+                PortEvent::ActiveInput { packet } => Some(
+                    self.ports
+                        .try_borrow()
+                        .map(|registry| registry.data_mode(packet.port, packet.data_mode))
+                        .unwrap_or(packet.data_mode),
+                ),
+                _ => None,
+            };
             // The term is built on the runtime's arena (the `RuntimeContext` the builders
             // need) *after* the registry borrow has been released — the same snapshot rule
             // the `-eval` poller follows. A driver-emitted term is stabilised the same way:
@@ -3516,11 +3529,20 @@
                     // recipient is the owner the drain already resolved, so a receiver blocked in
                     // `receive` is woken by the ordinary `deliver_in_table` call below (see
                     // [`ops::active_input`]).
-                    PortEvent::ActiveInput { packet } => (
-                        packet.owner,
-                        packet.port.to_eterm(),
-                        ctx.active_input_message(packet)?,
-                    ),
+                    PortEvent::ActiveInput { packet } => {
+                        let resolved = ops::active_input::ActiveInputPacket {
+                            port: packet.port,
+                            owner: packet.owner,
+                            data_mode: active_mode.unwrap_or(packet.data_mode),
+                            kind: packet.kind,
+                            event: packet.event.clone(),
+                        };
+                        (
+                            packet.owner,
+                            packet.port.to_eterm(),
+                            ctx.active_input_message(&resolved)?,
+                        )
+                    }
                     // A handled signal goes to the `erl_signal_server` process by name, with the
                     // emulator (`system`) as the sender. An unregistered name resolves to no pid,
                     // in which case there is nobody to notify and the signal is dropped.
@@ -3662,7 +3684,20 @@
                     if packet.owner != self.pid() {
                         continue;
                     }
-                    messages.push(self.active_input_message(&packet)?);
+                    let mode = self
+                        .runtime
+                        .ports
+                        .try_borrow()
+                        .map(|registry| registry.data_mode(packet.port, packet.data_mode))
+                        .unwrap_or(packet.data_mode);
+                    let resolved = ops::active_input::ActiveInputPacket {
+                        port: packet.port,
+                        owner: packet.owner,
+                        data_mode: mode,
+                        kind: packet.kind,
+                        event: packet.event.clone(),
+                    };
+                    messages.push(self.active_input_message(&resolved)?);
                 }
                 PortEvent::Signal { .. } => continue,
                 // A ready `tty_select/2` interest belongs in the **owning reader's** mailbox, which
```

Notes:
* `ActiveInputPacket`'s fields are `pub(crate)` and the type derives `Debug`, its `event` field is
  `Clone` (the e2e test already does `packet.event.clone()`), so the reconstruction compiles — this
  was **built, not guessed**.
* `try_borrow().unwrap_or(packet.data_mode)` never panics on a live borrow; it degrades to the
  open-time mode (the pre-O1 behaviour) for that one delivery.
* O1 was verified: `-p beam_rt --lib` builds with **zero warnings**, and with the §6 stand-ins
  neutralized both suites still run **25/25** and **4/4** (the e2e `packet.data_mode == Binary`
  assertions hold, because resolution happens only in the delivery arms).

**Take the core patch (11 hunks) first; add O1 in the same pass only if you want `{mode,list}` to
render like the oracle.** They are independent and O1 applies cleanly on top (verified with
`git apply --check`).

---

## 4. The closed set of `PortEvent` matches (requirement #5) — **exactly three need arms**

Grepped across `src/port.rs` and all of `src/port/**`:

| file:line | construct | needs an arm? |
|---|---|---|
| `src/port.rs:875-885` | `PortEvent::port()` — `match self` | **YES** (H3) |
| `src/port.rs:3398-3468` | `Runtime::deliver_port_messages` — `match &event` | **YES** (H10, and O1 rewrites it) |
| `src/port.rs:3536-3589` | `RuntimeContext::poll_port_messages` — `match event` (owned) | **YES** (H11) |
| `src/port.rs:2047-2055` | `drain_pending_events` — `.retain(\|event\| … event.port() …)` | no — uses `event.port()` |
| `src/port.rs:1668-1669` | `close`'s purge — `event.port() != Some(id)` | no — uses `event.port()` |
| `src/port/completion_tests.rs:632-637` | `filter_map(\|event\| match event { Completion … , _ => None })` | no — has `_` |
| `src/port/packet_framing_tests.rs:76-86` | `match event { Data …, Exited …, _ => {} }` | no — has `_` |
| `src/port/tests.rs:245-249` | `match event { Data …, Exited …, _ => {} }` | no — has `_` |
| `src/port/tests.rs:194`, `:1706` | `if let PortEvent::Data/Exited { … }` | no — `if let` |
| `src/port/accept_inherit_tests.rs:382` | `let PortEvent::Completion { packet } = event else { continue }` | no — `let-else` |
| `src/port/completion_tests.rs:799` | `pending_events.push(PortEvent::Completion { … })` | no — construction |

The `--all-targets` build with the patch applied compiled clean, which is the end-to-end proof that
this set is complete (test code included).

---

## 5. `note_inet_setopts` — the exact reply shape, and why it cannot double-record (requirement #6)

`PortRegistry::port_control(&mut self, id, op, arg) -> Result<Vec<u8>>` (`port.rs`, H6 site). The
driver's answer is the `Ok(bytes)`; a badarg/refusal is the `Err` arm. The hook is:

```rust
if ops::inet_opts::is_setopts_control(op) {
    if let Ok(bytes) = &reply {
        let _ = self.note_inet_setopts(id, arg, ops::inet_opts::setopts_succeeded(bytes));
    }
}
```

* **Reads `&reply`, never consumes it** — the `Err` path and the returned `reply` are untouched.
* `setopts_succeeded(&[u8])` is `reply.first() == Some(&INET_REP_OK /* 1 */)`
  (`inet_opts.rs:408-410`). A refused `setopts` is `Ok(b"\x00einval")` ⇒ `false` ⇒ `record` is called
  with `ok == false`; `InetOptions::record` (`inet_opts.rs:285-302`) **does not store** a rejected
  record. So a refused `setopts` records nothing.
* `is_setopts_control` (`inet_opts.rs:474-480`) matches the named verb `"setopts"` **or** the decimal
  `INET_REQ_SETOPTS + INET_CONTROL_MAGIC` (`7 + 0x03f1_a300 = 66327303`), so both of `prim_inet`'s
  request forms are covered.
* **Called once per successful `setopts`** — production is the one caller (`port_control`). Only the
  *tests* also call `note_inet_setopts` explicitly (the documented stand-in; see §6), which is why
  the count assertion doubles until that stand-in is removed.
* **No dedupe** is added inside `note_inet_setopts` (it is `inet_opts.rs`, out of patch scope, and
  dedupe would defeat `accept_inherit_tests.rs:705`'s deliberate duplicate-poisoning row, which
  writes a record the driver itself refuses so it is unreachable through the real path).

---

## 6. Companion test-helper changes required (OUT of this node's write scope — `.rs` test files)

The patch is **production-correct**; three test files were written with explicit calls that
**emulate** the hooks this patch installs. Once the hooks are live, those stand-ins must be removed,
otherwise they collide. This was proven empirically: applying only the core patch and running the
suites yields **10 failures in `port::ops::active_input`** and **1 in
`port::completion::accept_inherit`**; neutralizing the stand-ins (below) makes both green
(**25/25**, **4/4**).

The module docs themselves say so: `accept_inherit_tests.rs:199-201` calls `setopts` "the documented
stand-in for the (not yet wired, `src/port.rs`-owned) `note_inet_setopts` hook", and
`active_input_e2e_tests.rs:63-70` says "When the hook lands these explicit calls become redundant,
not wrong."

### 6.1 `src/port/accept_inherit_tests.rs:202-207` — the `setopts` helper double-records

```rust
// CURRENT (double-records with H6 live)
fn setopts(reg: &mut PortRegistry, id: PortId, arg: &[u8]) -> Vec<u8> {
    let reply = control(reg, id, &setopts_op(), arg);
    let ok = setopts_succeeded(&reply);
    reg.note_inet_setopts(id, arg, ok);
    reply
}
// REQUIRED after H6
fn setopts(reg: &mut PortRegistry, id: PortId, arg: &[u8]) -> Vec<u8> {
    control(reg, id, &setopts_op(), arg)
}
```
(The other three explicit `note_inet_setopts` calls in this file — lines `637`, `656`, `705` — must
**stay**: each drives a *refused* `setopts` through raw `control(...)`, so production records nothing
for them, and they assert the `note_inet_setopts` API itself.)

Symptom if not changed: `an_accepted_socket_inherits_the_listeners_option_block` panics at
`accept_inherit_tests.rs:478` — `left: 10, right: 5` (each of the five listener records stored
twice).

### 6.2 `src/port/active_input_e2e_tests.rs:389-397` — the same `setopts` stand-in

```rust
// CURRENT
fn setopts(&mut self, arg: &[u8]) -> Vec<u8> {
    let reply = self.control(&setopts_op(), arg);
    let ok = setopts_succeeded(&reply);
    let port = self.port;
    self.rt.ports.borrow_mut().note_inet_setopts(port, arg, ok);
    reply
}
// REQUIRED after H6
fn setopts(&mut self, arg: &[u8]) -> Vec<u8> {
    self.control(&setopts_op(), arg)
}
```

### 6.3 Reading delivered packets — the `drain_active_inputs()` assertions become `pending_events` reads

The suites assumed the provider drained nothing, so they called `PortRegistry::drain_active_inputs()`
**directly** after `write_command`/`pump`. With H4–H9 live, the staging sites consume the delivery
first, so a direct drain sees `[]`. These call sites must instead read the packet out of
`pending_events` (or out of `pump`'s returned events):

* `src/port/active_input_tests.rs:621`, `:641`, `:658`, `:683`, `:687`, `:712`, `:716`
  (tests `a_live_ports_delivery_is_bound_to_its_owner_mode_and_kind_at_drain_time`,
  `a_udp_ports_delivery_is_bound_to_the_udp_kind`,
  `a_delivery_for_a_port_that_has_left_the_runtime_is_dropped_once_and_never_re_reported`,
  `a_delivery_drained_before_a_close_is_still_deliverable_after_it`).
* `src/port/active_input_e2e_tests.rs:400-410` — the `round()` helper discards `pump`'s `_events`
  and then drains; it must collect the `PortEvent::ActiveInput` packets from `pump`'s returned
  events **and** from `pending_events`.

A minimal, verified helper shape for `active_input_tests.rs` (these are descendants of `port`, so the
private `pending_events` field and the private `PortEvent` enum are accessible, exactly as
`completion_tests.rs:632` already shows):

```rust
fn take_active_inputs(registry: &mut PortRegistry) -> Vec<ActiveInputPacket> {
    let mut out = Vec::new();
    let staged = std::mem::take(&mut registry.pending_events);
    let mut retained = Vec::new();
    for event in staged {
        match event {
            crate::port::PortEvent::ActiveInput { packet } => out.push(packet),
            other => retained.push(other),
        }
    }
    registry.pending_events = retained;
    out.extend(registry.drain_active_inputs());
    out
}
```
…and for `active_input_e2e_tests.rs::round()`, prepend the same `pending_events` drain **after**
collecting `PortEvent::ActiveInput` out of `pump`'s returned `events`.

I applied exactly these three neutralizations to the throwaway copy and both suites went green; **they
are not part of the `port.rs` patch** and are listed here only so the parent can hand them to the
test-file owner (or apply them in the same commit).

---

## 7. Verification recipe (run in this order)

Use the wrapper (sandbox `$HOME` is read-only); **always** pass
`--manifest-path erts_rust/Cargo.toml`. A bare `cargo` from the repo root fails. Use a generous
timeout (900 s) — a default timeout kills the build.

```bash
# 1. Build the patched crate: 33 dead-code warnings must become 0, and there must be no errors.
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --all-targets
#    BEFORE: "warning: `beam_rt` (lib) generated 33 warnings" (all dead-code: inet_opts + active_input)
#    AFTER : no `warning:` line at all, "Finished …"

# 2. The two target suites. THE FILTERS ARE THE FULL MODULE PATHS —
#    `port::ops::active_input` and `port::completion::accept_inherit`.
#    `-- port::active_input` silently runs ZERO tests (the module is `port::ops::active_input`).
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib -- port::ops::active_input
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib -- port::completion::accept_inherit

# 3. Whole port module (blast-radius check; the two pre-existing failures below are NOT patch-induced)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib -- port::
```

Expected results:

| step | before patch | after core patch (with §6 neutralized) |
|---|---|---|
| `build --all-targets` | 33 dead-code warnings, 0 errors | **0 warnings, 0 errors** |
| `-- port::ops::active_input` | `ok. 25 passed; 0 failed; … 1540 filtered out` | `ok. 25 passed; 0 failed` |
| `-- port::completion::accept_inherit` | `ok. 4 passed; 0 failed; … 1561 filtered out` | `ok. 4 passed; 0 failed` |
| `-- port::` | (not measured) | `300 passed; 2 failed` — both **pre-existing on the unpatched tree**, confirmed identical |
| `rustfmt` | — | `erts_rust/scripts/cargo.sh` has **no fmt subcommand** (it `exec cargo "$@"` after picking a writable `CARGO_HOME`), so `rustfmt --check` is not exposed here; the patch follows the file's existing style (4-space indent, 100-col wrap, `//` docs). |

**The two pre-existing `port::` failures (environment, not code):**
`port::driver::ddll_tests::an_open_port_spawn_driver_is_listed_by_erl_ddll_loaded_drivers` and
`port::driver::fire_tests::the_runtime_marks_a_dead_monitored_process_and_the_port_fires_process_exit`
both fail with `open_port: BadArg` on the **unpatched** tree too (the C driver fixture cannot be
built/loaded in this sandbox). Verified by running each filter against the baseline before applying
anything. They are outside this patch's surface.

---

## 8. What this unblocks

1. **`tests/inet_active.rs` — a `beam_rt`-binary end-to-end suite over real loopback.** Today it is
   *impossible* to write: nothing delivers a socket data message to a process, so `{active, …}`
   over the real CLI never produces a `{tcp, …}`/`{udp, …}`. With H1–H11 the drained packet reaches
   its owner's mailbox on both routes (scheduled `run` via `deliver_in_table`, `-eval` via
   `poll_port_messages`), so a suite can assert the oracle's `{tcp,#Port<…>,<<…>>}` through the
   actual binary, exactly as `tests/inet_completion.rs` already does for `{inet_reply,…}`.
2. **The `{active, N}` / `{active, once}` production path.** `beam_io` already implements the whole
   `{active, …}` state machine and stages deliveries (`drivers/inet/active.rs`); this patch is the
   missing consumer, so `gen_tcp`/`gen_udp` in any active mode finally deliver — including the
   `{tcp_passive, S}` / `{udp_passive, S}` counter-exhaustion message.
3. **Accepted-socket inheritance in a real run.** With H6 (`note_inet_setopts` from production), the
   `adopt_accepted → seed_inet_mode → inherit_inet_options` chain replays from a **populated** mirror
   instead of an empty one, so `gen_tcp:accept/1` on a configured listener produces a socket that
   really inherits `{packet,N}`/`{mode,…}`/`{header,N}`/watermarks.
4. **O1** additionally makes a `{mode, list}` active socket render a char list, matching the oracle.

---

## 9. Honest caveats / what is *not* verified

* **Compile verification is against a throwaway copy, not this worktree.** The core patch (11 hunks)
  and O1 (3 hunks) both build `-p beam_rt --lib` and `--all-targets` with **zero warnings / zero
  errors**, and both `git apply --check` cleanly onto a pristine copy of the current `src/port.rs`.
  They were never applied to the real worktree (this node may not edit `.rs`).
* **The suite green-ness depends on the §6 companion changes.** With the hooks live but the stand-ins
  left in place, 11 tests fail — all because the tests consume what production now consumes first.
  This is proven, not predicted.
* **The `{mode,list}` render is only correct with O1.** The core patch alone renders the open-time
  stream (binary). If you skip O1, record it as a known residual; do not claim `{mode,list}` parity.
* **Never re-derive `owner`/`port` in the delivery arms.** Both hunks read `packet.owner` /
  `packet.port` (requirement #3); a port closed between staging and delivery still renders its own
  port term, and `PortEvent::port() == None` (H3) is what keeps it out of `close`'s purge.
* `PortEvent` stays **non-`pub`** (it is `enum PortEvent` at `port.rs:733`); the new variant does not
  change that.

---

## 10. Applying it

```bash
# from the repo root, with the two diff blocks above saved as core.diff / o1.diff:
git apply --check core.diff && git apply core.diff
# optional (closes the {mode,list} render gap):
git apply --check o1.diff && git apply o1.diff
# then apply the §6 test-helper neutralizations in the same commit, then §7.
```
