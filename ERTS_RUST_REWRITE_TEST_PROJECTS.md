# ERTS/BEAM Rust Rewrite — Candidate Real-World Test & Benchmark Projects

Companion to `ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and `ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II).

The rewrite docs define the **compatibility surface** (9 contracts) and prescribe a
portfolio of benchmark **categories** (compute/JIT-bound, messaging/copy-bound,
GC-bound, ETS-bound, I/O-bound, distribution-bound, hot-upgrade, startup/p99 tail),
but they deliberately name **no concrete suite**. This document fills that gap with a
deliberately small list: only workloads that are worth running *during development*
without the benchmark itself consuming engineering time.

Only **pure Erlang/Elixir** workloads are meaningful for evaluating a rewritten VM. A
target that drags in an external database server, an ML/XLA compiler, or a heavy
native/foreign-runtime stack measures the surrounding system, not the VM — so
full-stack deployments (Postgres + Phoenix) and native-backed pipelines (Nx/EXLA) are
out of scope.

Selection rule: **popular** *and* **pure Erlang/Elixir** *and* **easy and fast to
benchmark** — a turnkey in-repo benchmark or a trivially runnable one, with low setup.

## 1. Primary workloads (default dev loop)

- **EMQX** (Erlang MQTT broker; EMQ) — https://github.com/emqx/emqx
  - Tests: EUnit + Common Test across a mono-repo of Erlang apps.
  - Benchmark: first-party `emqtt-bench` (Erlang, MQTT v5) load generator.
  - Stresses: messaging/copy boundary, distribution/ETF, TLS via `crypto`/`ssl` NIFs, `epoll` I/O, scheduler/tail latency.

- **Khepri** (Erlang tree DB on Ra/Raft; built for RabbitMQ) — https://github.com/rabbitmq/khepri
  - Tests: `rebar3 eunit`, `rebar3 proper`, `rebar3 ct`.
  - Benchmark: a per-commit CI benchmark that measures **call throughput, memory footprint and GC counts** for single-node and 3-node clusters (queries, writes, deletes).
  - Stresses: `ordered_set`/tree lookups, GC pressure, disk I/O via dirty schedulers, distribution. Small, self-contained, and directly charts GC numbers — the ideal first target.

- **The Computer Language Benchmarks Game — Erlang programs** — https://benchmarksgame-team.pages.debian.net/benchmarksgame/
  - Pure compute kernels (n-body, binary-trees, mandelbrot, …), zero dependencies, instant to run.
  - Benchmark: the parallel project **hanabi1224/Programming-Language-Benchmarks** wraps the same kernels with **CI-run correctness tests + benchmarks**.
  - Stresses: straight-line kernels — the cleanest source of JIT/loop-optimization workloads; compiling the kernels in-tree also exercises the "self-hosted build" contract.

## 2. Compiler throughput (Erlang and Elixir compilers)

A first-class, essentially free benchmark: compile a fixed corpus and time it.

- **Targets**: the Erlang compiler (`erlc` / the `compile` module) and the Elixir compiler (`mix compile` / `elixirc`).
- **Corpus**: the retained projects' own Erlang/Elixir source trees, plus OTP's own compiler corpus (`lib/compiler/test`) and the wider `lib/*/test`.
- **Protocol**: compile cold vs warm; single-file vs parallel (`erlc -j` / `make -j`).
- **Metrics**: wall-clock, files/sec, LOC/sec, peak memory, and compiler startup.
- **Why it fits**: cheap, deterministic, dependency-free, and it directly stresses `erl_parse`, `erl_lint` and the `beam_ssa_*` passes, and exercises the listed "self-hosted build" compatibility contract. A single well-known project tree can serve as the compilation corpus, so this runs essentially for free alongside the workloads above.

## 3. Cross-cutting tooling

- **Benchee** (Elixir micro-benchmark framework) — https://github.com/bencheeorg/benchee — the natural way to add per-function micro-benchmarks to any Elixir workload.
- **Erlang/OTP's own suites** (this repo: `erts/emulator/test`, `lib/*/test`, `lib/compiler/test`) — not a third-party project, but the canonical differential-correctness corpus.

## Reserve / heavier-setup alternatives

Popular, but heavier to stand up. Keep them in reserve for deeper validation — **not** in the recommended fast dev loop:

- **RabbitMQ** (Erlang AMQP broker) — `rabbitmq/rabbitmq-server`; large Common Test suites; first-party `rabbitmq-perf-test`.
- **MongooseIM** (Erlang XMPP server) — `esl/MongooseIM`; Common Test + PropEr; first-party `amoc` load platform.
- **ejabberd** (Erlang XMPP/MQTT/SIP) — `processone/ejabberd`; Common Test regression suites; community load tooling.
- **VerneMQ** (Erlang MQTT broker) — `vernemq/vernemq`; Common Test suites; MQTT load generation.
- **Riak KV** (Erlang distributed KV store) — `basho/riak`; EUnit/Common Test; the `basho/basho_bench` load generator.

## Coverage matrix (project → primary rewrite surface stressed)

| Project | Msg/copy | Distrib/ETF | ETS | GC | I/O | JIT/compute | NIF ABI | Hot-upgrade | Tests | Bench |
|---|---|---|---|---|---|---|---|---|---|---|
| EMQX | ●● | ●● | ● | ○ | ●● | ○ | ●● (ssl/crypto) | ● | ✔ | ✔ (`emqtt-bench`) |
| Khepri | ● | ●● | ●● | ●● | ●● | ○ | ○ | ○ | ✔ | ✔ (CI bench) |
| Benchmarks Game kernels | ○ | ○ | ○ | ● | ○ | ●● | ○ | ○ | ✔ (hanabi1224) | ✔ |
| Compiler throughput (`erlc` / `mix compile`) | ○ | ○ | ○ | ● | ● (file I/O) | ●● | ○ | ○ | ✔ (`lib/compiler/test`) | ✔ |
| OTP suites (`erts/emulator/test`, `lib/*/test`) | ● | ● | ● | ● | ● | ● | ● | ● | ✔ | ○ |

Legend: ●● primary, ● secondary, ○ not a focus, ✔ present/available.

## Notes / gaps

- Per the rewrite docs, **no single workload covers the surface** — use the small
  portfolio above and run everything in both `emu` and `jit` flavors for differential
  comparison, and measure **startup + p99 tail latency** on every run (they are hard
  correctness gates, not nice-to-haves).
- The docs mandate a **differential compatibility harness against real ERTS** as the
  gating safety net (Stage 0). The workloads above exercise it; the harness itself is
  in-tree work.
- Weight the first targets toward **Khepri**, the **Benchmarks Game kernels** and the
  **compiler benchmark**: densest signal, least setup.
