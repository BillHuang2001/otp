# ERTS/BEAM Rust Rewrite — Candidate Real-World Test & Benchmark Projects

Companion to `ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and `ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II).

The rewrite docs define the **compatibility surface** (9 contracts) and prescribe a
portfolio of benchmark **categories** (compute/JIT-bound, messaging/copy-bound,
GC-bound, ETS-bound, I/O-bound, distribution-bound, hot-upgrade, startup/p99 tail),
but they deliberately name **no concrete suite**. This document fills that gap: a
short list of production Erlang/Elixir projects that each ship **both a functional
test suite and a benchmark harness**, so the same code can be used to (a) check
correctness of a new VM and (b) measure performance against real ERTS.

Selection rule: real-world/production code, an in-repo or first-party **test suite**,
and an accessible **benchmark** (in-repo bench dir, a first-party load tool, or a
published repeatable methodology).

## 1. Messaging / network-bound (exercises the copy boundary, distribution/ETF, I/O, schedulers, crypto/ssl NIFs)

- **EMQX** (Erlang MQTT broker; EMQ) — https://github.com/emqx/emqx
  - Tests: EUnit + Common Test across a mono-repo of Erlang apps.
  - Benchmarks: official `emqtt-bench` (Erlang, MQTT v5) + the Open MQTT Benchmark Suite.
  - Stresses: millions of connections, small-envelope process messaging/copy boundary, distribution, TLS via `crypto`/`ssl` NIFs, `epoll` I/O, scheduler/tail latency.

- **RabbitMQ** (Erlang AMQP broker; Broadcom/VMware) — https://github.com/rabbitmq/rabbitmq-server
  - Tests: one of the largest Common Test suites in the ecosystem (per-plugin suites under `deps/*/test`).
  - Benchmarks: `rabbitmq-perf-test` (first-party), plus published repeatable AMQP 1.0 benchmark methodology (`rabbitmq.com/blog/2024/08/21/amqp-benchmarks`).
  - Stresses: `gen_server`/`gen_statem` envelope traffic, ETS/Mnesia/Khepri metadata store, distribution, `crypto`/`ssl`, plugin code loading / hot upgrade.

- **MongooseIM** (Erlang XMPP server; Erlang Solutions) — https://github.com/esl/MongooseIM
  - Tests: extensive Common Test + PropEr + the first-party `escalus` client test framework.
  - Benchmarks: first-party `amoc` load-testing platform with ready-made scenarios.
  - Stresses: distribution, messaging, `ssl`, socket I/O at scale.

- **VerneMQ** (Erlang MQTT broker) — https://github.com/vernemq/vernemq
  - Tests: Common Test suites.
  - Benchmarks: MQTT load-generation (used directly in the EMQ Open MQTT Benchmark Suite, which includes VerneMQ).
  - Stresses: network/distribution, connection churn.

- **ejabberd** (Erlang XMPP/MQTT/SIP; ProcessOne) — https://github.com/processone/ejabberd
  - Tests: Common Test-driven regression suites (origin of `escalus`).
  - Benchmarks: load-testing tooling / community load tests.
  - Stresses: distribution, multi-protocol messaging, `ssl`.

## 2. Storage / database-bound (exercises ETS incl. ordered_set, GC pressure, dirty-I/O schedulers, distribution)

- **Khepri** (Erlang tree DB on Ra/Raft; built for RabbitMQ) — https://github.com/rabbitmq/khepri
  - Tests: `rebar3 eunit`, `rebar3 proper`, `rebar3 ct` (per its README).
  - Benchmarks: a dedicated per-commit benchmark tool on CI that measures **call throughput, memory footprint and GC counts** for single-node and 3-node clusters (queries, writes, deletes).
  - Stresses: `ordered_set`/tree lookups, GC pressure, disk I/O via dirty schedulers, distribution. Small, self-contained, and directly charts GC numbers — ideal first target.

- **Riak KV** (Erlang distributed KV store; community-maintained) — https://github.com/basho/riak
  - Tests: EUnit/Common Test suites.
  - Benchmarks: the widely-used **`basho_bench`** load-generation tool (Erlang) — https://github.com/basho/basho_bench
  - Stresses: distribution, ETS, I/O, `gen_server` traffic.

- **CouchDB** (document DB; Apache) — https://github.com/apache/couchdb
  - Tests: Elixir-based integration/Regressions suite.
  - Benchmarks: published community benchmark studies (e.g. Hovercraft disk/index benchmarks; neighbourhood.ie 10x-case-study methodology).
  - Stresses: HTTP concurrency, GC, distribution between nodes.

## 3. Elixir applications (language semantics plus high-level pipelines, PubSub, ETS)

- **Phoenix + Supabase Realtime** (Elixir WebSocket/PubSub at scale)
  - https://github.com/phoenixframework/phoenix — ExUnit suite; framework benchmarks / connection tests.
  - https://github.com/supabase/realtime — ExUnit suite; benchmark harnesses for connection/message throughput.
  - Stresses: process messaging via `Phoenix.PubSub` (PG2 process groups), distribution, concurrency, `Enum`/`Stream` pipelines.

- **Oban** (Elixir background-job processing) — https://github.com/oban-bg/oban
  - Tests: extensive ExUnit suite.
  - Benchmarks: `bench/` harness plus published job-throughput benchmarks (e.g. BullMQ-vs-Oban).
  - Stresses: ETS, messaging, GC, DB I/O.

- **Nx / EXLA** (Elixir tensors + XLA backend) — https://github.com/elixir-nx/nx
  - Tests: ExUnit suites (`nx`, `exla`).
  - Benchmarks: first-party `nx/bench` and `exla/bench`.
  - Stresses: **compute-bound straight-line kernels** (the best JIT/loop-opt candidate class), and the **NIF ABI** by construction (EXLA via Rustler/`crypto`-style native boundary).

## 4. Cross-cutting benchmark harnesses & kernel suites

- **Benchee** (Elixir micro-benchmark framework) — https://github.com/bencheeorg/benchee
  - A benchmark *tool* (with its own ExUnit tests), used inside most Elixir projects above; the natural way to add per-function benchmarks to any candidate.
- **basho_bench** (Erlang load-generation tool) — https://github.com/basho/basho_bench
  - Drives the Riak KV benchmarks; a general Erlang load generator.
- **amoc** (Erlang/Elixir load-testing framework, MongooseIM) — https://github.com/esl/amoc
- **The Computer Language Benchmarks Game — Erlang programs** — https://benchmarksgame-team.pages.debian.net/benchmarksgame/
  - Pure compute kernels (n-body, binary-trees, mandelbrot, etc.). No first-party test suite, but the parallel project **hanabi1224/Programming-Language-Benchmarks** wraps the same kernels with **correctness tests + benchmarks** and is CI-run — the cleanest source of straight-line-kernel workloads for the JIT/loop-optimization and FBIP/fusion evaluation.
- **Erlang/OTP's own suites** (in this repo: `erts/emulator/test`, `lib/*/test`, `lib/compiler/test`)
  - Not a third-party project, but the canonical differential correctness corpus (run the 35-app tree and the emulator/compiler suites against the new VM), and the place the rewritten VM must self-host.

## Coverage matrix (project → primary rewrite surface stressed)

| Project | Msg/copy | Distrib/ETF | ETS | GC | I/O | JIT/compute | NIF ABI | Hot-upgrade | Tests | Bench |
|---|---|---|---|---|---|---|---|---|---|---|
| EMQX | ●● | ●● | ● | ○ | ●● | ○ | ●● (ssl/crypto) | ● | ✔ | ✔ (`emqtt-bench`) |
| RabbitMQ | ●● | ●● | ●● | ● | ● | ○ | ●● | ●● | ✔ | ✔ (perf-test) |
| MongooseIM | ●● | ●● | ● | ○ | ●● | ○ | ● (ssl) | ● | ✔ | ✔ (amoc) |
| ejabberd | ●● | ●● | ● | ○ | ● | ○ | ● | ● | ✔ | ▲ (load tools) |
| VerneMQ | ●● | ●● | ● | ○ | ●● | ○ | ● | ● | ✔ | ▲ (load tools) |
| Khepri | ● | ●● | ●● | ●● | ●● | ○ | ○ | ○ | ✔ | ✔ (CI bench) |
| Riak KV | ● | ●● | ●● | ● | ●● | ○ | ○ | ○ | ✔ | ✔ (basho_bench) |
| CouchDB | ● | ●● | ● | ●● | ●● | ○ | ○ | ○ | ✔ | ▲ (studies) |
| Phoenix/Realtime | ●● | ●● | ● | ● | ● | ○ | ● (Rustler) | ○ | ✔ | ▲/✔ |
| Oban | ● | ● | ●● | ●● | ● | ○ | ○ | ○ | ✔ | ✔ (bench/) |
| Nx/EXLA | ○ | ○ | ○ | ● | ○ | ●● | ●● (EXLA) | ○ | ✔ | ✔ (bench/) |
| Benchmarks Game (Erlang) | ○ | ○ | ○ | ● | ○ | ●● | ○ | ○ | ✔ (hanabi1224) | ✔ |

Legend: ●● primary, ● secondary, ○ not a focus, ▲ benchmark exists but less turnkey.

## Notes / gaps

- Per the rewrite docs, **no single project covers the surface** — use a portfolio and
  run everything in both `emu` and `jit` flavors for differential comparison, and
  measure **startup + p99 tail latency** on every run (they are hard correctness
  gates, not nice-to-haves).
- The docs mandate a **differential compatibility harness against real ERTS** as the
  gating safety net (Stage 0). The projects above are the *workloads*; the harness
  itself is in-tree work.
- Weight the first targets toward small, self-contained, benchmark-instrumented
  projects: **Khepri** (GC/ETS/throughput, CI-published numbers) and **Nx/EXLA**
  (compute kernels + NIF ABI) give the densest signal per unit of setup effort.
