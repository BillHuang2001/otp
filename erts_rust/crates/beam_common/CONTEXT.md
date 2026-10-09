# beam_common — Workspace Foundation (lowest common ancestor)

## Intent
The single shared foundation crate of the Rust ERTS rewrite (Stage 0 — Foundations, see
`../../CONTEXT.md` §7). Every other crate depends on it, so its public API must be **small, stable and
idiomatic**. It owns the workspace-wide **error/result strategy** (Part I §A.1; Part II §B: BIF/NIF
failure is **return-based** — no `setjmp`/`longjmp`), the OTP/ERTS **version constants**, the
runtime **configuration / feature-flag / flavor skeleton**, a dependency-free **logging facade**,
the **canonical definitions of ERTS constants genuinely shared by two or more workspace crates**
(`constants`, currently `CONTEXT_REDS`), and the documented **`unsafe` marker convention**.

This is a **dependency sink**: it depends on `std` (and `libc` for ABI-sized integer aliases) and on
**nothing else** — deliberately no `thiserror`, no `tracing`, no `bitflags`. Adding any dependency
here must be justified in `../../CONTEXT.md` and `../../../CHANGE.md`.

## API Surface
Re-exported at the crate root (`beam_common::…`); modules are private except `safety`.

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
- Files target ~1000 lines; split by responsibility (`error`, `version`, `config`, `logging`).

## Known Issues
- **The `FeatureFlags` surface is largely inert.** Only `smp` is consumed anywhere (reported by
  `beam_rt`'s `system_info(smp_support)`, `info/system.rs:102`); `interpreter`, `jit`, `cranelift`,
  `dirty_schedulers`, `lock_checking`, `debug_assertions` and `memory_instrumentation` are set by the
  builders / `for_flavor` but read by no crate. In particular `-emu_flavor emu` only flips the reported
  `emu_flavor` atom (`for_flavor` sets `features.jit = false`, which nothing branches on) — it does not
  disable the JIT.
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
  - `logging`: level round-trip (`set_level`/`level`/`enabled`); `init_from_env` default when unset.
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
- `src/logging.rs` → dependency-free logging facade + log macros
- `src/safety.rs` → the workspace `unsafe` marker convention (docs only)
- `src/lib.rs` → crate docs, module wiring, root re-exports

## See Also
- `../../CONTEXT.md` → workspace contracts (error strategy §3, staging §7, dependency-sink rule).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` (Part I §A) and
  `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II §B/§H) → the compatibility surface & staged plan.
