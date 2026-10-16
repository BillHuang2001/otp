# m14 external-target notes

All external targets are **sys-only** (`Runtimes::SysOnly`) and are driven through the read-only
`$TMPDIR` temp-copy adapter — the source repos at `/home/bill/Source/{cowboy,khepri,emqx,elixir,Programming-Language-Benchmarks}`
are never modified.

| target | outcome | figure / reason |
|---|---|---|
| `erlc_compiler` | `ok` | 6.038 files/s, 4 680 loc/s (60 files, wall 9 938 ms) |
| `elixir_compiler` | `ok` | 1.825 files/s, 916.3 loc/s (223 files, wall 122 200 ms, startup 943.6 ms) — installed system `elixirc` |
| `cowboy` | `ok` | 30 690 req/s, 153 600 requests, 0 errors, p50 0.29 / p99 3.41 ms, build 81 790 ms |
| `khepri` | `ok` | `rebar3 eunit` wall 97 530 ms |
| `plb` | `ok` | wall 113 900 ms; test p50 626.4 / p99 3 939 ms; test mem 88.83 MB |
| `emqx` | `skipped(emqx-requires-otp-28)` | checkout pins OTP 28.5.0.7-2; host is OTP 29.1.1 |

`plb` resolved **dotnet 9.0.318** (`/run/current-system/sw/bin/dotnet`, PATH) via
`tool::command_for_min(TOOL, TOOL_NIX_ATTR, MIN_DOTNET_SDK = &[9])`, rejecting two 8.0.425 SDKs
(`.../dotnet-sdk-8.0.425/...`, `.../dotnet-sdk-wrapped-8.0.425/...`) with `requires >= 9` — the
rejection notes are recorded on the row.

`emqx`'s skip note names the pin (`env.sh:3`, `.tool-versions:1`) and both verified blockers; the
enablement ladder is `m12-emqx-enablement-attempt.md`. No number is fabricated.
