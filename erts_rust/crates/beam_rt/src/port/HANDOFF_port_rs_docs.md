# HANDOFF — `src/port.rs` rustdoc rewrite (doc comments only)

## Why this file exists
`src/port.rs` is the **sibling file** of this node (`src/port/`), so this agent's write scope stops at the
directory. The doc rewrites below belong to `src/port.rs` and must be applied by an agent whose node covers
`./erts_rust/crates/beam_rt/src` (or the file itself). Everything is **doc-comment text only**: no code,
signature or behaviour change. `src/port/CONTEXT.md` (inside this node) is already rewritten to the same
current facts and committed.

## The one fact every replacement states
`beam_io`'s in-tree `ram_file_drv` hands each reply up through `DriverContext::deliver_read`
(`beam_io/src/drivers/ram_file.rs:477`, `ctx.deliver_read(&reply.header, reply.body.as_deref())`), so one
`driver_output*` call is one `ReadMessage { header, body }` and the C's split survives end to end.
Consequence (measured on a real `{spawn_driver,"ram_file_drv"}` port, both streams): `OK`/`NUMBER`/`ERROR`
are the proper header list (`driver_output2(hbuf, hlen, NULL, 0)`, `erts/emulator/drivers/common/ram_file_drv.c:181-220`),
`DATA` on a `[binary]` port is the improper `[2,0,0,0,2|<<97,98>>]` (`driver_output_binary`, `:448-489`), and
a `[list]` port is the flat char list. Pinned by `port::ram_file` 6/6, `port::driver_output_shape` 7/7 and
`port::stream_shape` 7/7 (all green).
`StreamShape::DriverOutput` therefore only decides a **header-less** read.
The only in-tree producers still on the flat `DriverContext::input()` sink are `tcp_inet`'s and
`udp_inet`'s passive `recv` verb (`beam_io/src/drivers/inet/async.rs:676`,
`beam_io/src/drivers/inet/udp.rs:933`); `spawn.rs`/`fd.rs` push flat too but are `PortName::Spawn`/`Fd`
(`StreamShape::Data`), not built-in driver ports.

## 1. Module doc (`//!`), lines 91-96 — replace

```text
//! A **built-in driver port**'s replies (`{spawn_driver, "ram_file_drv"}` and friends) come up
//! through [`beam_io`]'s **flat** byte queue instead, which carries no split, so they are rendered
//! as list elements on either stream — the C's header rendering ([`StreamShape::DriverOutput`]).
//! That is exact for a header-only reply — every `ram_file_drv` `OK`/`NUMBER`/`ERROR` — and is the
//! documented residual for a reply that carried a data part, whose binary tail the `beam_io` seam
//! does not carry.
```

with

```text
//! An **in-tree driver**'s reply carries its own split: `ram_file_drv` hands every reply up as one
//! [`ReadMessage`] ([`beam_io::DriverContext::deliver_read`],
//! `beam_io/src/drivers/ram_file.rs:477`), so a header-only `OK`/`NUMBER`/`ERROR` is the proper list
//! (`[3,0,0,0,0]` for a fresh `open`) and a `DATA` reply the improper `[2,0,0,0,2|<<97,98>>]` on a
//! `[binary]` port — the oracle's own terms, on either stream.
//!
//! A **header-less** read of a built-in driver port is the one shape the [`StreamShape`] decides
//! instead: it renders as list elements on either stream — the C's header rendering
//! ([`StreamShape::DriverOutput`], `erts/emulator/beam/io.c:3404-3435`) — which is the answer for the
//! two in-tree producers still on [`beam_io::DriverContext::input`], `tcp_inet`'s and `udp_inet`'s
//! passive `recv` verb (`beam_io/src/drivers/inet/{async,udp}.rs`), where the C answers a binary on a
//! `[binary]` port.
```

## 2. `PortName::stream_shape` doc, lines 363-369 — replace

```text
    /// The [`StreamShape`] this `Name` form's reads carry — the producer fact a read cannot    /// say about itself.
    ///
    /// A **built-in** driver port's bytes come up through [`beam_io::DriverContext::input`], a
    /// flat byte queue that cannot say which `driver_output*` call wrote them, so they render
    /// the C's header half ([`StreamShape::DriverOutput`]). Every other producer states its own
    /// header/body split in the read ([`StreamShape::Data`]).
```

(note the joined `///` on the first line — a typo to fix while replacing) with

```text
    /// The [`StreamShape`] this `Name` form's reads carry — the producer fact a read cannot say
    /// about itself.
    ///
    /// A **built-in** driver port renders through [`StreamShape::DriverOutput`], because its
    /// header-less reads are the ones [`beam_io::DriverContext::input`] — the only sink with no
    /// split attached — still feeds. Every other producer states its own header/body split in the
    /// read ([`StreamShape::Data`]): a child's stdout, an `fd` port, a `dlopen`ed driver's
    /// `driver_output*`.
```

## 3. `StreamShape` doc, lines 438-441 — replace

```text
/// What a read cannot say is whether its bytes came from a driver output call **at all**: a
/// **built-in** driver port hands its bytes up through [`beam_io::DriverContext::input`], a flat
/// byte queue with no split attached, so an in-tree driver's reply arrives as one header-less read.
/// [`StreamShape::DriverOutput`] says exactly that.
```

with

```text
/// What a read cannot say is whether its producer stated a header: a header-less read is one body and
/// no header whether it came from a plain `driver_output` or from [`beam_io`]'s flat
/// [`beam_io::DriverContext::input`] sink, which carries no split at all.
/// [`StreamShape::DriverOutput`] supplies exactly that one producer fact, and only for a header-less
/// read.
```

## 4. `StreamShape::DriverOutput` variant doc, lines 454-476 — replace

```text
    /// A **built-in driver port**'s payload: an in-tree driver ([`PortName::SpawnDriverBuiltin`] —
    /// `ram_file_drv`, `tcp_inet`, `udp_inet`) answered through one of the four `driver_output*`
    /// entry points, and [`beam_io`]'s flat byte queue does not say which.
    ///
    /// A **header-less** read of such a port renders as **list elements on either stream** — what
    /// the C gives the header half of every output call (`deliver_read_message`,
    /// `erts/emulator/beam/io.c:3404-3435`), so a `[binary]` port's `{data, D}` is a list of byte
    /// integers rather than a binary.
    ///
    /// That rendering is **exact** when the driver answered header-only — `driver_output2(hbuf,
    /// hlen, NULL, 0)`, which is every `ram_file_drv` `OK`/`NUMBER`/`ERROR`
    /// (`ram_file_drv.c:181-228`) — and is the documented residual otherwise: a call that carried
    /// a **data part** (a `DATA` reply, `driver_output_binary`, `:448`) has its binary tail
    /// rendered as further list elements (`[2,0,0,0,2,97,98]` where the oracle delivers
    /// `[2,0,0,0,2|<<97,98>>]`), and a header-less `driver_output` (an `inet` socket read) is
    /// rendered as list elements where the oracle delivers a binary. Closing that residual needs
    /// an in-tree driver to hand its bytes up through [`beam_io::DriverContext::deliver_read`]
    /// instead of [`beam_io::DriverContext::input`] — a `beam_io` change, the handoff in this
    /// node's `CONTEXT.md`.
    ///
    /// A read that **does** carry a header contradicts the flat queue, so its own split wins (see
    /// [`StreamShape::Data`]): the shape never hides bytes a producer really delivered as a header.
```

with

```text
    /// A **built-in driver port**'s **header-less** read: an in-tree driver
    /// ([`PortName::SpawnDriverBuiltin`] — `ram_file_drv`, `tcp_inet`, `udp_inet`) produced a read
    /// with no header, the shape every push through [`beam_io`]'s flat
    /// [`beam_io::DriverContext::input`] sink takes.
    ///
    /// Such a read renders as **list elements on either stream** — what the C gives the header half
    /// of every output call (`deliver_read_message`, `erts/emulator/beam/io.c:3404-3435`), so a
    /// `[binary]` port's `{data, D}` is a list of byte integers rather than a binary.
    ///
    /// That rendering is the oracle's **exact** proper list for a header-only reply, and diverges
    /// where the C answers a binary on a `[binary]` port: the producers still on that flat sink are
    /// `tcp_inet`'s and `udp_inet`'s passive `recv` verb
    /// (`beam_io/src/drivers/inet/{async,udp}.rs`), whose payload is one body-only read.
    ///
    /// A read that **does** carry a header keeps its own split (see [`StreamShape::Data`]), whatever
    /// the port's `Name` form: `ram_file_drv` hands its `OK`/`NUMBER`/`ERROR` up header-only and its
    /// `DATA` reply with both halves, so both render the oracle's own terms.
```

## 5. `PortMeta::kind` field doc, lines 539-543 — replace

```text
    /// The two message producers route on it (see [`PortName::stream_shape`] and
    /// [`PortMeta::has_driver_control`] for the `port_control` gate's `Name`-form half): a
    /// built-in driver port's bytes come up through [`beam_io::DriverContext::input`], a flat byte
    /// queue that cannot say which `driver_output*` call wrote them, while a `dlopen`ed
    /// `{spawn_driver, Name}` port's reads carry the C's header/body split.
```

with

```text
    /// The two message producers route on it (see [`PortName::stream_shape`] and
    /// [`PortMeta::has_driver_control`] for the `port_control` gate's `Name`-form half): a built-in
    /// driver port's **header-less** reads render as list elements
    /// ([`StreamShape::DriverOutput`]), while a `dlopen`ed `{spawn_driver, Name}` port's reads carry
    /// the C's header/body split.
```

## 6. `PortEvent::Data::shape` field doc, lines 766-770 — replace

```text
        /// The producer-specific rendering rule: [`StreamShape::DriverOutput`] for a built-in
        /// driver port's flat payload (rendered as list elements on either stream),
        /// [`StreamShape::Data`] for every other producer here (a child's stream bytes, an `fd`
        /// port, a `dlopen`ed driver's `driver_output`) and for every reassembled `{packet, N}`
        /// packet.
```

with

```text
        /// The producer-specific rendering rule: [`StreamShape::DriverOutput`] for a built-in
        /// driver port, which decides only a **header-less** read (rendered as list elements on
        /// either stream), [`StreamShape::Data`] for every other producer here (a child's stream
        /// bytes, an `fd` port, a `dlopen`ed driver's `driver_output`) and for every reassembled
        /// `{packet, N}` packet.
```

## 7. `RuntimeContext::make_stream_payload` doc, last bullet at lines 4489-4492 — replace

```text
    /// * a **built-in driver port**'s **header-less** read ([`StreamShape::DriverOutput`]) is list
    ///   elements on **either** stream — the C's header rendering, and the one shape the producer's
    ///   flat byte queue cannot carry. A read that does carry a header keeps its own split (see
    ///   that variant).
```

with

```text
    /// * a **built-in driver port**'s **header-less** read ([`StreamShape::DriverOutput`]) is list
    ///   elements on **either** stream — the C's header rendering (`io.c:3404-3435`). A read that
    ///   carries a header keeps its own split (see that variant), so `ram_file_drv`'s header-only
    ///   `OK`/`NUMBER`/`ERROR` and its two-halved `DATA` reply render the oracle's own terms.
```

## 8. `PortRegistry::command` doc, lines 1638-1645 — replace

```text
    /// That round is also what a **built-in** driver answers from: `ram_file_drv`/`efile` write
    /// their reply into [`beam_io::DriverContext::input`] during the `OUTPUT` task, and the round
    /// harvests it as a [`beam_io::PortNotification::Data`] — draining the port's input queue, so
    /// no later round can see it. The outcome is therefore **folded into**
    /// [`PortRegistry::pending_events`] (through the same [`PortRegistry::fold_notifications`] the
    /// pump uses) rather than dropped, and the next [`PortRegistry::pump`] reports it as the
    /// owner's `{Port, {data, Data}}`. Dropping it was the defect: the reply never reached the
    /// owner and `port_info(Port, input)` stayed `0`.
```

with

```text
    /// That round is also what a **built-in** driver answers from: `ram_file_drv` hands its reply up
    /// as one completed read ([`beam_io::DriverContext::deliver_read`]) during the `OUTPUT` task, and
    /// the round harvests it as a [`beam_io::PortNotification::Data`] — draining the port's read
    /// queue, so no later round can see it. The outcome is therefore **folded into**
    /// [`PortRegistry::pending_events`] (through the same [`PortRegistry::fold_notifications`] the
    /// pump uses), and the next [`PortRegistry::pump`] reports it as the owner's
    /// `{Port, {data, Data}}` with both byte counters advanced.
```

## Verification recipe
```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --all-targets   # warning-free
erts_rust/scripts/cargo.sh test  --manifest-path erts_rust/Cargo.toml -p beam_rt -- port::stream_shape port::driver_output_shape port::ram_file
rg -n "flat byte queue|cannot say which|three further asks" erts_rust/crates/beam_rt/src/port.rs   # must be empty
```
`cargo doc -p beam_rt --no-deps` is **not** warning-free at baseline (348 crate-level warnings, 92 of them in
`src/port*`, all the pre-existing "links to private item" / "both a function and a module" class); the
replacement blocks only link items the surrounding docs already link, so they add none.
