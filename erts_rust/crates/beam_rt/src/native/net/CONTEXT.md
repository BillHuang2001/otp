# `beam_rt/src/native/net/` — the distribution node-set, connector and Erlang-supervisor floor (A.5)

## Intent
The node-facing calls that read or drive this runtime's own **link table**: `erlang:nodes/0,1,2`,
`net_adm:ping/1`, `net_kernel:connect_node/1`, `erl_distribution:start_link/0,1` and
`erlang:monitor_node/2`.
They are state no `beam_bifs` module can reach — exactly like the process/registry queries in
`../process.rs` — so they are served from the runtime-native floor, consulted **before** the
`beam_bifs` registry.
The node's *identity* (`is_alive/0`, `get_cookie/0`, `set_cookie/2`) lives in `../node.rs`; the links
themselves are `crate::dist`/`crate::dist_client`'s, and their A.5 contract is `../../dist/CONTEXT.md`'s.
The dispatch is `../native.rs`'s (the one dispatch table — this node adds no second list); `net/mod.rs`'s
module docs are the authoritative per-call contract.

## API Surface
### `erlang:nodes/0,1,2` — `nodes.rs`
`nodes/0` is the C's `nodes(visible)`: the runtime's own established outbound links in connection
order (`Runtime::dist_connected_nodes`), `[]` for a runtime that is not a node.
`nodes/1` folds one atom or a list of them into five flags — `visible`/`hidden`/`connected` select
peers, `this`/`known` select the local node — and answers **peers first, the local node last**,
de-duplicated in first-seen order (measured: after one peer, `nodes([this,connected])` is
`[peer, local]`); any other element, a non-atom/non-list argument or an improper tail is `badarg`
(the C's `goto badarg`).
`nodes/2` is the same node set answered as `{Node, Info}` pairs; the second argument must be a **map**
whose only recognised keys are `connection_id` and `node_type` valued `true`/`false`, the number of
recognised keys must equal the map size (so any unrecognised key is `badarg`), a non-map is `badarg`,
and `Info` is a **flat** map holding `connection_id` then `node_type` in that key order.
`connection_id`'s value is the link's **real per-link integer** (`Runtime::dist_connection_id`),
stable for that link's lifetime and different per peer — never a capability gap — or the atom
`undefined` for a `this` entry; `node_type` is `this`/`visible`.
On a dormant node (`nonode@nohost`): `nodes/0,1` peer selectors ⇒ `[]`, `nodes(this)`/`nodes(known)` ⇒
`[nonode@nohost]`, `nodes(visible, #{connection_id => true})` ⇒ `[]`, `nodes(this, #{})` ⇒
`[{nonode@nohost, #{}}]`.
### The connector — `connector.rs`
`net_adm:ping/1` ⇒ `pong`/`pang`; `net_kernel:connect_node/1` ⇒ `true`/`false`; on a runtime that is
**not** a node they answer `pang` and `ignored` without dialling (`net_kernel.erl`'s `connect_node/1`
clause tests `is_alive()` — the oracle's own answer, measured for `nonode@nohost` and for an arbitrary
atom — *not* the `false` a failed connection reports).
Both dial through `Runtime::dist_connect`, i.e. the client side of the `'N'` handshake via `epmd`, and
two details of the real connector are reproduced here rather than in `dist_client`:
* **A full `name@host` is dialled through its short `name`** — this OTP release registers a node with
  `epmd` under its short name whether it was started `-sname foo` or `-name foo@host` (`epmd -names`),
  so a verbatim `name@host` lookup found nothing (`noport`).
* **A dial failure is the BIF's own answer, never a raise** — `pang`/`false` for every unreachable peer
  (`nosuchnode@nowhere`, an `epmd`-unknown name, a peer that refuses the handshake); the typed error
  `Runtime::dist_connect` returns **is** that answer, and its diagnostic stays observable through
  `DistClient::last_error`.
`net_kernel:connect_node(node())` ⇒ `pong`/`true` without any handshake.
### `erl_distribution:start_link/0,1` — the Erlang-level distribution supervisor
`kernel_sup`'s distribution child is `{erl_distribution, start_link, []}` (`kernel.erl:262-268`), the
same entry `erl_distribution:start/1` invokes from `net_kernel:start/1`; both start a supervisor whose
children are the **Erlang socket stack** (`net_kernel:protocol_childspecs/0`, `erl_epmd`, `auth`,
`net_kernel`), which cannot run here — there is no `prim_inet`/`inet_tcp` driver, and the distribution
is the native `crate::dist::DistNode` the CLI binds and publishes to `epmd` itself.
Both keys answer the atom **`ignore`** (how `erl_distribution` expresses "nothing to start":
`do_start_link([]) -> ignore` with no `-sname`/`-name` argument) and both are in
`floor_keys("erl_distribution")`, so the namespace is key-granular (`init/1`, `start/1`, `stop/0` stay
ordinary `.beam` bodies).
The measured registered-name divergence is only `net_sup`/`auth` (`undefined` here, live pids on the
oracle), because **`net_kernel` IS registered** on a distributed node — by this crate's own
`net_kernel` service (`../net_kernel.rs`), which is what answers a real peer's `net_adm:ping/1`
(`gen:call` `{is_auth, Node}` ⇒ `yes`).
### `erlang:monitor_node/2` — `monitor.rs`
One latch per subscribing process over `Runtime::node_monitors` (`monitor_node_add`/`monitor_node_remove`),
emitted by the scheduler pass's `Runtime::service_node_monitors`; the body only decides the argument
shapes and seeds the latch with the node's state **now**, which is what implements the no-replay rule.
The answer is the atom `true` for **every atom** `Node` (`node()` and `nonode@nohost` included, and a
node never connected to), `badarg` only for a non-atom `Node` or a `Bool` that is not `true`/`false`;
subscribing to an **already** connected node yields no immediate `nodeup`, and a `nodedown` arrives when
the node's last link retires.
`monitor_node/3` (the `Info`-carrying form) is deliberately not served — it stays `beam_bifs`'s typed
capability gap.

## Constraints
- `#![deny(unsafe_code)]` (crate-wide); terms through `beam_engine::terms`; return-based errors — a
  `Node`/`Options` rejection is `BeamError::BadArg`, and **a non-atom argument is `badarg` here where
  the oracle raises `function_clause`** (a native BIF cannot raise it), the honest substitute rather
  than a fabricated answer.
- **Own-state limits** (never a fabricated value): `nodes(known)` reports this runtime's own `this` (the
  oracle's extra `not_connected` entries are not modelled) and `hidden` links are not modelled, so
  `nodes(hidden)` is always `[]`.
- The latch store's `node` field is an atom **immediate** — the node monitors never hold a heap word.

## Test Strategy
- `tests.rs` is the colocated suite: the dormant-node answer matrix for `nodes/0,1,2`, the argument
  grammar (`badarg` shapes, improper tails, malformed option maps), `net_adm:ping/1` ⇒ `pang`,
  `connect_node/1` ⇒ `ignored`, `erl_distribution:start_link/0` ⇒ `ignore`, and the
  `floor_keys("erl_distribution")` key-granularity pin.
- Live two-VM evidence for the same families (a real `erl` peer and a second `beam_rt`) is
  `../../tests/{live_signals,dist_two_vm,dist_two_rust,dist_two_vm_wire,node_cookie}.rs`; those are
  `#[ignore]`d and their recipe is `../../tests/CONTEXT.md`'s.

## Routing Table
- `mod.rs` → the module docs (the contract every sibling's rustdoc defers to), the dormant-node answer
  table and the one helper the four siblings share.
- `nodes.rs` → `erlang:nodes/0,1,2`: the selector grammar, the `NodeInfoOptions`/`NodeEntryType` shapes
  and the information map.
- `connector.rs` → `net_adm:ping/1`, `net_kernel:connect_node/1`, `erl_distribution:start_link/0,1` and
  the two shared peer-name helpers.
- `monitor.rs` → `erlang:monitor_node/2` (the store and the notifications are `crate::Runtime`'s).
- `tests.rs` → the colocated suite.
- `../node.rs` → the node identity/cookie surface; `../net_kernel.rs` → the registered `net_kernel`
  service; `../../dist.rs` + `../../dist/` + `../../dist_client/` → the links the connector drives.

## See Also
- `../CONTEXT.md` → the floor's dispatch order, the constraints and the not-implemented index.
- `../../dist/CONTEXT.md` → the live distribution contract (peer loss, control ordering, the idempotent
  connect).
- `../../../M1_GAPS.md` *Distribution and the node (A.5)* → the divergence ledger.
- `../../../../../erts/emulator/beam/dist.c` → `nodes`, `erts_monitor_nodes` (read-only ground truth).
