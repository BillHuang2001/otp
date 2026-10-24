# beam_common — Workspace Foundation (lowest common ancestor)

## Intent
The single shared foundation crate of the Rust ERTS rewrite (Stage 0 — Foundations, see
`../../CONTEXT.md` §7). Every other crate depends on it, so its public API must be **small, stable and
idiomatic**. It owns the workspace-wide **error/result strategy** (Part I §A.1; Part II §B: BIF/NIF
failure is **return-based** — no `setjmp`/`longjmp`), the OTP/ERTS **version constants**, the
runtime **configuration / feature-flag / flavor skeleton**, a dependency-free **logging facade**,
the **canonical definitions of ERTS constants genuinely shared by two or more workspace crates**
(`constants`, currently `CONTEXT_REDS`), the **one local reference-id space** every locally minted
reference term draws from (`refid`), the **single RFC 1321 MD5** the whole workspace shares
(`md5`), and the documented **`unsafe` marker convention**.

This is a **dependency sink**: it depends on `std` and on **nothing else** — deliberately no
`thiserror`, no `tracing`, no `bitflags`. Adding any dependency here must be justified in
`../../CONTEXT.md` and `../../../CHANGE.md`.

## API Surface
Re-exported at the crate root (`beam_common::…`); modules are private except `safety` and `md5`
(the MD5 module is public so downstream crates can also write `beam_common::md5::{Md5, digest}`).

- `error` — `BeamError` (the ONE workspace error type), `Result<T>`, and the term-independent
  exception machinery: `ExceptionClass`, `ExitReason`, `Raise`, `LiveReason`, `StackTrace`,
  `StackFrame`, `SourceLocation`, `AtomName`, `AllocFailure`, `AllocKind`.
  - `pub type Result<T> = core::result::Result<T, BeamError>;`
  - `BeamError` variants: `BadArg`, `BadArith`, `BadArity`, `BadMatch`, `SystemLimit`,
    `Alloc(AllocFailure)`, `TermNotFound`, `Io(std::io::Error)`, `Raise(Raise)`.
  - `Raise { class, reason, stacktrace, live_reason }` (all fields public). `live_reason:
    Option<LiveReason>` is the **opaque live-reason slot**: `ExitReason::Structured` is a unit variant,
    so a structured reason's live *term* travels out-of-band here instead of in the reason. Builders
    `new`/`error`/`exit`/`throw`/`with_stacktrace`/`with_live_reason`, accessor `live_reason()`.
    `Debug`+`Clone`+`PartialEq`+`Eq` are kept, but `Debug`, `PartialEq` and `Eq` are **manual**:
    equality compares only the observable parts (`class`/`reason`/`stacktrace`) and ignores the slot,
    and `Debug` prints the placeholder `<live reason>` for it — so the payload is never printed,
    dereferenced or compared. `Clone` stays derived (the handle is an `Arc`).
  - `LiveReason` — the opaque handle: `new<T: Any + Send + Sync + 'static>(T)`,
    `downcast_ref::<T>(&self) -> Option<&T>`, `as_any(&self) -> &dyn Any`; `Clone` + `Send` + `Sync`;
    `Debug` prints `<live reason>` (never the payload).
  - **Live-reason convention (for adopters):** a raising crate with a term representation stores its
    live reason term as an `Eterm` payload (`LiveReason::new(term)`) and attaches it with
    `Raise::with_live_reason`; a consumer recovers it with `downcast_ref::<Eterm>()`. `beam_common`
    names no term type — that is the adopter's payload type, not a dependency of this crate.
  - `#![forbid(unsafe_code)]` in this crate; `AtomName` wraps `Cow<'static, str>` (no interning,
    no term dependency).
- `version` — `OTP_VERSION` ("30.0-rc0"), `ERTS_VERSION` ("17.1"), `ERTS_VSN` ("erts-17.1"),
  `Version`/`Prerelease` (parse + `Display`), `ParseVersionError`, `otp_version()`, `erts_version()`.
- `config` — `Flavor` (`Emu`/`Jit`), `FeatureFlags`, `RuntimeConfig` (skeleton over the A.6 CLI/env
  surface), `LogLevel`.
  `RuntimeConfig` holds only *typed* emulator values (flavor/log level/node name/cookie/dist port
  range/scheduler counts/max_processes) and preserves unmodeled emulator flags verbatim in
  `extra_emulator_flags`; it does **not** retain the raw command line or the `init` argument list,
  so `init:get_argument/1`/`get_arguments/0` cannot be served from it — the raw boot argv must be
  captured (and split into `{Flag, Values}` groups) by `beam_rt` (matching `init.erl`'s
  `parse_boot_args/1`).
- `constants` — the canonical cross-crate ERTS constants: `CONTEXT_REDS` (`= 4000`), the ERTS
  reduction quantum / context-switch threshold (`erts/emulator/beam/erl_vm.h:47`). This module is the
  designated home for any ERTS constant genuinely shared by ≥2 crates; a value used by a single crate
  stays in that crate. See *Cross-Crate Constant Ownership*.
- `refid` — the **one local reference-id space**: `next_ref_id() -> u64`, a process-wide monotonic
  id source over one private `static NEXT: AtomicU64` (`Ordering::Relaxed` `fetch_add`; first value
  `0`; total — it never panics and its wrap point is unreachable in practice). Every reference
  *term* minted locally draws its `id` word from it: `erlang:make_ref/0`, the NIF `make_ref`,
  `erlang:monitor/2,3`, `erlang:alias/1`, timer references, `seq_trace` references and the NIF
  **resource terms** of `enif_make_resource`. Term-independent by design (this crate names no term
  type) — the small-int encoding of an id is the *caller's* check. See *Cross-Crate Reference Ids*.
- `md5` — the **single RFC 1321 MD5** of the workspace: `BLOCK_LEN` (`= 64`), `DIGEST_LEN` (`= 16`),
  the one-shot `digest(&[u8]) -> [u8; DIGEST_LEN]` and the streaming `Md5`
  (`new`/`update`/`finish`/`from_parts` plus the raw-state accessors
  `state`/`total_len`/`buffered_len`/`buffer`; `Default`, `Clone`, `Debug`, `PartialEq`, `Eq`).
  `finish(&self)` pads a *copy* of the state, so it is pure and re-finishable; `from_parts` stores
  the full 64-byte buffer verbatim and rejects `len > BLOCK_LEN` with `BeamError::BadArg`. See
  *Cross-Crate MD5*.
- `logging` — dependency-free facade: `init_logging`, `init_from_env`, `set_level`, `level`,
  `enabled`, plus the `beam_trace!`/`beam_debug!`/`beam_info!`/`beam_warn!`/`beam_error!` macros.
- `safety` — documentation-only module stating the `unsafe` convention (see Constraints).

## Constraints
- **Return-based errors only.** No `setjmp`/`longjmp`, no silent swallowing, no panic-as-control-flow.
  Fallible operations return `Result<T, BeamError>`; the engine turns a `BeamError::Raise` (or a
  reason error like `BadArg`) into the actual BEAM raise (exception tag in `x(0)`).
- **No dependency on `beam_term`.** `ExitReason`/`Raise`/`AtomName` are **term-independent**; the
  raising crate maps them to live terms. A structured (tuple/list/map) reason is shown here as
  `ExitReason::Structured` (unit variant, no term), and its live term rides along out-of-band in the
  opaque `Raise::live_reason` slot — never as a `beam_term` dependency.
- **Stability:** public enums/structs are `#[non_exhaustive]`; construct via `Default`/`new` +
  builder setters. `BeamError` implements `Display` + `std::error::Error` (source = the inner
  `io::Error`); `From<std::io::Error>` is provided.
- `LogLevel` ordering is `Debug < Info < Warning < Error < Silent`; the configured level is the
  most verbose level emitted, so `enabled(l)` is true when `l >= level()` and `l != Silent`
  (`Debug` enables everything; `Silent` enables nothing, so `enabled(Silent)` is always false).
- Every public item (incl. fields and variants) is documented — `missing_docs` is denied.
- **`unsafe` convention** (owned here, binding on all crates): `unsafe` is confined to explicitly
  marked modules; every `unsafe` block/`fn` carries a preceding `// SAFETY: <invariant/rationale>`
  comment; any `unsafe` gains a Miri/fuzz or differential test where feasible. `beam_common` itself
  contains none (`#![forbid(unsafe_code)]`).
- Files target ~1000 lines; split by responsibility (`error`, `version`, `config`, `constants`,
  `refid`, `logging`, `md5`).

## Cross-Crate Constant Ownership
`constants` is the single owner of ERTS constants genuinely shared by **two or more** workspace
crates; a value used by a single crate stays in that crate. `beam_common` is the lowest common
ancestor, so dependents import/re-export from here instead of transcribing their own copy.

- **`CONTEXT_REDS`** (`= 4000`, `erts/emulator/beam/erl_vm.h:47`) — the ERTS reduction quantum /
  context-switch threshold; a frozen ERTS interface constant reported by
  `erlang:system_info(context_reductions)`. Consumed by three sibling crates, which derive from this
  definition rather than transcribing their own copy:
  `beam_io::port_task::CONTEXT_REDS` (`crates/beam_io/src/port_task.rs`, `usize`) is a re-export;
  `beam_rt::info::schedulers` (`crates/beam_rt/src/info/schedulers.rs`, `i64`) reports
  `system_info(context_reductions)` from it; and `beam_process::DEFAULT_REDUCTIONS_PER_SLICE`
  (`crates/beam_process/src/scheduler.rs`, `u32`, the per-process slice budget) is `CONTEXT_REDS as u32`.

Other genuinely cross-crate-duplicated ERTS constants proved during the audit. Each already has (or
needs) a canonical owner **outside** `beam_common`; adopters listed — **none edited from this node**:

- **`MAP_SMALL_MAP_LIMIT`** (`= 32`, `erts/emulator/beam/erl_map.h:96`) — canonical
  `beam_term::MAP_SMALL_MAP_LIMIT` (`crates/beam_term/src/map.rs`); copies to drop:
  `crates/beam_engine/src/terms/mod.rs`, `crates/beam_bifs/src/maps.rs`.
- **`MAX_ARG`** (`= 255`, `erts/emulator/beam/erl_vm.h:48`) — published twice already
  (`beam_term::fun::MAX_ARG`, `beam_process::registers::MAX_ARG`); copies to drop:
  `crates/beam_abi/src/nif/enif/process.rs` (`c_int`), `crates/beam_bifs/src/fun_info.rs` (`i64`),
  `crates/beam_engine/src/interpreter/dispatch.rs` (`usize`),
  `crates/beam_loader/src/chunk/table.rs` (`i32`) and
  `crates/beam_loader/src/lower/validate.rs` (`u32`).
- **`MAX_REG`** (`= 1024`, `erts/emulator/beam/erl_vm.h:49`) — canonical `beam_process::MAX_REG`
  (`crates/beam_process/src/registers.rs`); copy to drop: `crates/beam_loader/src/decode.rs` (`u64`).
- **`MAX_ARITYVAL`** (`= (1<<24)-1`, `erts/emulator/beam/erl_term.h:322`) — canonical
  `beam_term::MAX_ARITYVAL` (`crates/beam_term/src/tag.rs`); copy to drop:
  `crates/beam_codec/src/encode/mod.rs`.
- **`MAX_ATOM_CHARS`** (`= 255`, `erts/emulator/beam/atom.h:29` `MAX_ATOM_CHARACTERS`) — no canonical
  Rust owner yet; duplicated in `crates/beam_codec/src/decode/mod.rs` and twice in `beam_bifs`
  (`src/convert.rs`, `src/binconv.rs`).
- **`VERSION_MAGIC`** (`= 131`, `erts/emulator/beam/external.h:84`) — canonical
  `beam_codec::VERSION_MAGIC`; `beam_dist` already re-exports it (no duplicate).
- Minor / trivially derived (noted, not actionable here): `NSEC_PER_SEC`
  (`beam_abi/src/nif/enif/time.rs`, `beam_io/src/cpu_time.rs`); `WORD_BYTES` / `U32_PER_WORD`
  (recomputed from `size_of::<Eterm>()` per crate); `POS_BIG_SUBTAG` / `REF_SUBTAG`
  (canonical in `beam_term::tag`; only `#[cfg(test)]` copies elsewhere).

## Cross-Crate MD5
`md5` is the workspace's **one** RFC 1321 MD5. It lives here because this crate is the lowest common
ancestor of its three consumers: the `.beam` code checksum of contract **A.2** (what
`Module:module_info(md5)` reports, `erts/emulator/beam/beam_file.c:1753-1829`), the distribution
handshake digest of **A.5** (`lib/erl_interface/src/connect/ei_connect.c`) and the
`erlang:md5/*` BIF family of **A.7** (`md5/1`, `md5_init/0`, `md5_update/2`, `md5_final/1`).

Only the arithmetic lives here: one-shot `digest`, streaming `Md5`, and the raw-state accessors
(`state`/`total_len`/`buffered_len`/`buffer`) with their inverse `from_parts`, so `beam_bifs` can
marshal the byte-exact 96-byte `erts_md5_state` image (`erts/emulator/beam/erl_md5.h:30-35`) in and
out without duplicating a single round — the wire layout stays a BIF-layer concern. `Md5` is that
struct field for field, including the C's buffering asymmetry (a whole 64-byte update on an empty
buffer hashes immediately, two 32-byte updates leave a full *unhashed* buffer) and the `pad` zero
run that only stays non-negative because it is written as `(56 + 64 - filled) & 63`. `finish` takes
`&self` and pads a copy, which is what makes `erlang:md5_final/1` pure.

Dependency-free (it does not even use this crate's `libc`), safe (`#![forbid(unsafe_code)]`),
allocation-free (fixed arrays only) and panic-free; pinned by the RFC 1321 Appendix A.5 suite plus a
length-`0..=130` / pad-boundary sweep whose goldens are oracle-derived.

## Cross-Crate Reference Ids
A reference term is the on-heap block
`[make_header(3, REF_SUBTAG), node = small(0), serial = small(0), id = small(N)]`, so its
**identity is the `id` word alone** — `node`/`serial` are constant in M1 (one node, one local
space).
`refid` is that one space, and **no crate keeps a private reference-id counter** — every locally
minted reference term's `id` comes from `beam_common::next_ref_id()`, so two producers can never hand
out the same id.
The `id`-only identity above is why: an overlapping space would make a **dead** NIF resource term
word-indistinguishable from a live plain reference, so an identity-matching reachability predicate
(the runner's park-arm resource prune) would retain the dead object and a liveness/monitor check
could treat it as live.

Every producer draws from it:
- `erlang:make_ref/0`, the NIF `make_ref`, monitor references minted by the runtime, aliases, timer
  references and `seq_trace` references, through `beam_rt`'s `Runtime::refs`
  (`crates/beam_rt/src/runtime.rs` — a `RefAllocator`);
- monitor/alias references through `beam_process::RefAllocator`, a **zero-sized handle** onto the
  same counter rather than a counter of its own (`crates/beam_process/src/monitor.rs`);
- NIF resource terms (`enif_make_resource` and the `enif_select` magic ref) through
  `crates/beam_abi/src/nif/enif/resources.rs`, which narrows the drawn `u64` to the block's
  `make_small` id slot **checked** against `beam_term::MAX_SMALL` — an unrepresentable id is a typed
  refusal that publishes no term and takes no reference, never a truncated id.

The space lives here because this crate is the lowest common ancestor: `beam_abi` (wave 0, which
mints resource terms) cannot host something `beam_process` (wave 3, monitors/aliases) draws from, and
`beam_process` must not depend on the NIF ABI crate.
The typed wrappers stay with their owners (`beam_process::RefAllocator`/`MonitorRef`, `beam_abi`'s
checked resource mint); `beam_common` only hands out the id.
`beam_process::MonitorRef::from_id` is the one **explicit-id, non-mint** constructor — it draws
nothing and its uniqueness is the caller's claim; its one workspace use is the unnamed-ETS-table tid,
whose id must be the table handle so the tid decodes back through `beam_bifs::access::ref_thing_id`
(`crates/beam_rt/src/runtime/ets_owner.rs`).
The one-space invariant is pinned by `crates/beam_rt/tests/ref_id_space.rs` (all producers
interleaved), `crates/beam_process/tests/reference_ids.rs` (a draw *is* the shared counter's next id)
and `crates/beam_abi/src/nif/enif/resources_tests.rs` (the resource mint's draw).

Two documented boundaries: (a) the counter is **not** bounded to the small-int range here (this
crate must not depend on `beam_term`) — a caller writing the id via `make_small` range-checks the
encoding itself and refuses an id past `MAX_SMALL` (2^59) rather than truncating it
(`BeamError::SystemLimit` for a monitor reference, `None` from the NIF resource mint), a bound
unreachable in practice and far below the counter's wrap point; (b) ids
decoded from **another node** (ETF, `beam_codec`) are not from this space — real ERTS separates
those with `node`/`serial`, which M1 keeps at `0`, so the guarantee is over *locally minted* ids.

## Known Issues
- **The `FeatureFlags` surface is largely inert.** Only `smp` is consumed anywhere (reported by
  `beam_rt`'s `system_info(smp_support)`, `crates/beam_rt/src/info/system.rs`); `interpreter`, `jit`,
  `cranelift`, `dirty_schedulers`, `lock_checking`, `debug_assertions` and `memory_instrumentation`
  are set by the builders / `for_flavor` but read by no crate. In particular `-emu_flavor emu` only
  flips the reported `emu_flavor` atom (`for_flavor` sets `features.jit = false`, which nothing
  branches on) — it does not disable the JIT.
- **`RuntimeConfig::log_level` and `RuntimeConfig::dist_port_range` are never read** outside this crate
  (set + tested here only). `log_level` is not wired to the logging facade — the effective level comes
  solely from `ERTS_RUST_LOG`/`set_level`/`init_from_env`, so `with_log_level` has no runtime effect.
- **Unused public helpers:** `RuntimeConfig::is_jit` and `erts_version()` are referenced only by this
  crate's own tests; no other crate calls them.

## Test Strategy
- In-module `#[cfg(test)] mod tests` only (no fixture/data needs).
  - `error`: `Display` for every `BeamError` variant and reason; `From<io::Error>`; `source()`;
    `as_atom()` mapping; `Raise` builders + `ExitReason` display; `AtomName` conversions;
    `LiveReason` round-trip + wrong-type `downcast_ref`/`as_any`; `Raise` slot survives `Clone`;
    `PartialEq` ignores the slot but still splits `class`/`reason`/`stacktrace`; `Debug` hides the
    payload and `Display` is unchanged; a slot-less raise behaves as before and a `Structured` raise
    with a slot still yields `as_atom() == None`.
  - `version`: parse/round-trip `OTP_VERSION`/`ERTS_VERSION`, prerelease parse, `Display`, and the
    rejection of malformed input (`ParseVersionError`).
  - `config`: `Default` values, `Flavor` parse/`as_str`, `FeatureFlags::for_flavor`, builder setters,
    `LogLevel` parse + ordering.
  - `constants`: `CONTEXT_REDS == 4000` (the frozen `erl_vm.h:47` value).
  - `refid`: successive draws strictly increase; 4 threads × 512 concurrent draws are all distinct
    and advance the one shared counter by exactly the draw count (a single contiguous run of ids, so
    there is no per-caller space); a later draw exceeds every earlier one. The module's tests
    serialize on a private `TEST_LOCK` because they share the process-wide counter and assert exact
    draw counts.
  - `logging`: level round-trip (`set_level`/`level`/`enabled`); `init_from_env` default when unset.
  - `md5`: the RFC 1321 Appendix A.5 suite (`""`, `"a"`, `"abc"`, `"message digest"`, the two
    alphabets/numerals cases, the 10^6-`a` vector) plus the `b""`/`[b'A'; 64]`/`[b'A'; 100]` byte
    anchors; the pad-boundary sweep over the `patterned` corpus (`i % 251`) at **every** length
    `0..=130` plus `1000` and `4096`, one-shot *and* streamed in 7-byte chunks, with the coverage of
    the `54..=63` window and the explicit `55/56/57/62/63/64/65/119/120/127/128/1000/4096` list
    asserted; `digest(d) == Md5::new().update(d).finish()` at every one of those lengths; split
    invariance over `[0, 1, 63, 64, 65, 100, 128, 199, 200]`; `finish` purity (twice-finishable, the
    state untouched, a later `update` still counts); the `from_parts`/accessor round trip including
    the `len == 64` image, the rebuilt-from-parts context, the verbatim non-zero buffer tail and the
    `len > BLOCK_LEN` → `BeamError::BadArg` refusal; and the C buffering asymmetry (`len == 0` after
    one 64-byte update, `len == 64` unhashed after two 32-byte ones).
- Doctests: every public item carries a runnable example (`cargo test` runs them; do **not** pass
  `--lib` to the crate gate, which would skip them).
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_common` GREEN.
  Full-workspace `cargo test --workspace` must stay green as dependents land.

## Notes for Agents
- This crate is a **leaf node** — it has no child subdirectories, only source files.
- Build/test from the repo root with an explicit manifest path (do not rely on `cd`):
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_common`.
- Public API changes here ripple through every crate: keep additions additive and justified.
- `libc` is declared in `Cargo.toml` (intended for ABI-sized integer aliases) but is currently referenced by no source file — it is a manifest-only dependency today.

## Routing Table
- `src/error.rs` → `BeamError`/`Result` + term-independent exception machinery (`Raise`, `ExitReason`, …)
- `src/version.rs` → OTP/ERTS version constants, `Version` parsing/`Display`
- `src/config.rs` → `Flavor`, `FeatureFlags`, `RuntimeConfig`, `LogLevel`
- `src/constants.rs` → canonical cross-crate ERTS constants (`CONTEXT_REDS`)
- `src/refid.rs` → the one local reference-id space (`next_ref_id`), shared by every locally minted
  reference term (plain refs, monitors/aliases, timers, `seq_trace`, NIF resource terms)
- `src/logging.rs` → dependency-free logging facade + log macros
- `src/md5.rs` → RFC 1321 MD5: one-shot `digest`, streaming `Md5` (`BLOCK_LEN`/`DIGEST_LEN`) and the
  raw-state accessors + `from_parts` that let `beam_bifs` marshal the `erts_md5_state` image
- `src/safety.rs` → the workspace `unsafe` marker convention (docs only)
- `src/lib.rs` → crate docs, module wiring, root re-exports

## See Also
- `../../CONTEXT.md` → workspace contracts (error strategy §3, staging §7, dependency-sink rule).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` (Part I §A) and
  `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II §B/§H) → the compatibility surface & staged plan.
