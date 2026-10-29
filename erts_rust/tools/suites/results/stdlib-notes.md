# OTP `stdlib` `common_test` suites under the system ERTS vs the Rust runtime (`beam_rt`)

Per-case sweep of eight `lib/stdlib/test` `common_test` suites driven by `erts_rust/tools/suites`
(`data/ct_runner.erl`, which expands `Suite:all()` through `groups/0` and prints one
`CASE <name> OK|SKIP|EXC|TIMEOUT|MISSING` line per case).

## A. Environment

| item | value |
| --- | --- |
| oracle | `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — OTP release/erts `29/17.1.1` |
| rust runtime | `/home/bill/Source/otp/.genesis/workers/worker_T1_A595/erts_rust/target/release/beam_rt` — version `30/17.1`, tier default (baseline JIT) |
| rust runtime git sha (measured) | `ec44fa3f7fc7d8419ae32f11b23a06448b565ec7` |
| suites | 8 — dict_SUITE, lists_SUITE, maps_SUITE, proplists_SUITE, queue_SUITE, binary_module_SUITE, string_SUITE, unicode_SUITE |
| `--jobs` | 4 |
| nproc | 16 |
| host loadavg before | `1.17 8.41 29.97 2/857 5` |
| host loadavg after | `1.56 5.70 22.41 2/865 977` |
| per-case timeout of the canonical run | 300 s (suite budget 330 s) |

Commands (repo root):

```
CARGO_HOME=${CARGO_HOME:-$HOME/.cache/cargo-home} cargo build --manifest-path erts_rust/tools/suites/Cargo.toml
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --release
cd erts_rust/tools/suites && ./target/debug/suites run --target stdlib \
  --suite lists_SUITE,maps_SUITE,proplists_SUITE,dict_SUITE,queue_SUITE,binary_module_SUITE,string_SUITE,unicode_SUITE \
  --jobs 4 --out results/stdlib
```

Artifacts: `results/stdlib/report.json` (schema `suites/1`) + `results/stdlib/summary.md`;
raw per-case logs and the extra probes quoted below are under `results/stdlib/raw/`.

## B. Per-suite case tables

`sys`/`rust` cells are `status (passed/failed)`; `timeout`/`error` are harness statuses with no
pass/fail split. The last column is the verified root-cause class (see §D for the reduction).

### dict_SUITE (7 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `create` | pass (1/0) | pass (1/0) | agree | — |
| `doctests_dict` | pass (1/0) | pass (1/0) | agree | — |
| `doctests_gb_trees` | pass (1/0) | pass (1/0) | agree | — |
| `doctests_orddict` | pass (1/0) | pass (1/0) | agree | — |
| `iterate` | pass (1/0) | pass (1/0) | agree | — |
| `remove` | pass (1/0) | pass (1/0) | agree | — |
| `store` | pass (1/0) | pass (1/0) | agree | — |

Totals: cases 7 · sys 7 pass / 0 fail · rust 7 pass / 0 fail

### lists_SUITE (78 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `append_1` | pass (1/0) | pass (1/0) | agree | — |
| `append_2` | pass (1/0) | fail (0/1) | divergence | heap-cap → `system_limit` (BIF heap 16 Mi words) |
| `doctests` | pass (1/0) | pass (1/0) | agree | — |
| `droplast` | pass (1/0) | pass (1/0) | agree | — |
| `dropwhile` | pass (1/0) | pass (1/0) | agree | — |
| `enumerate` | pass (1/0) | pass (1/0) | agree | — |
| `filter_partition` | pass (1/0) | pass (1/0) | agree | — |
| `flatten_1` | pass (1/0) | pass (1/0) | agree | — |
| `flatten_1_e` | pass (1/0) | pass (1/0) | agree | — |
| `flatten_2` | pass (1/0) | pass (1/0) | agree | — |
| `flatten_2_e` | pass (1/0) | pass (1/0) | agree | — |
| `hof` | pass (1/0) | pass (1/0) | agree | — |
| `join` | pass (1/0) | pass (1/0) | agree | — |
| `keymember` | pass (1/0) | pass (1/0) | agree | — |
| `keymerge` | pass (1/0) | pass (1/0) | agree | — |
| `keyreplace` | pass (1/0) | pass (1/0) | agree | — |
| `keysearch_keyfind` | pass (1/0) | pass (1/0) | agree | — |
| `keysort_1` | pass (1/0) | pass (1/0) | agree | — |
| `keysort_error` | pass (1/0) | pass (1/0) | agree | — |
| `keysort_i` | pass (1/0) | pass (1/0) | agree | — |
| `keysort_rand` | pass (1/0) | pass (1/0) | agree | — |
| `keystore` | pass (1/0) | pass (1/0) | agree | — |
| `keytake` | pass (1/0) | pass (1/0) | agree | — |
| `member` | pass (1/0) | pass (1/0) | agree | — |
| `merge` | pass (1/0) | pass (1/0) | agree | — |
| `otp_5939` | pass (1/0) | pass (1/0) | agree | — |
| `otp_6023` | pass (1/0) | pass (1/0) | agree | — |
| `otp_6606` | pass (1/0) | pass (1/0) | agree | — |
| `otp_7230` | pass (1/0) | pass (1/0) | agree | — |
| `reverse` | pass (1/0) | pass (1/0) | agree | — |
| `rkeymerge` | pass (1/0) | pass (1/0) | agree | — |
| `rmerge` | pass (1/0) | pass (1/0) | agree | — |
| `rukeymerge` | pass (1/0) | pass (1/0) | agree | — |
| `rumerge` | pass (1/0) | pass (1/0) | agree | — |
| `search` | pass (1/0) | pass (1/0) | agree | — |
| `seq_2` | pass (1/0) | pass (1/0) | agree | — |
| `seq_2_e` | pass (1/0) | pass (1/0) | agree | — |
| `seq_3` | pass (1/0) | pass (1/0) | agree | — |
| `seq_3_e` | pass (1/0) | pass (1/0) | agree | — |
| `seq_loop` | pass (1/0) | pass (1/0) | agree | — |
| `sort_1` | pass (1/0) | pass (1/0) | agree | — |
| `sort_2` | pass (1/0) | pass (1/0) | agree | — |
| `sort_rand` | pass (1/0) | pass (1/0) | agree | — |
| `sublist_2` | pass (1/0) | pass (1/0) | agree | — |
| `sublist_2_e` | pass (1/0) | pass (1/0) | agree | — |
| `sublist_3` | pass (1/0) | pass (1/0) | agree | — |
| `sublist_3_e` | pass (1/0) | pass (1/0) | agree | — |
| `subtract` | pass (1/0) | fail (0/1) | divergence | heap-cap → `system_limit` (BIF heap 16 Mi words) |
| `suffix` | pass (1/0) | pass (1/0) | agree | — |
| `takewhile` | pass (1/0) | pass (1/0) | agree | — |
| `ukeymerge` | pass (1/0) | pass (1/0) | agree | — |
| `ukeysort_1` | pass (1/0) | pass (1/0) | agree | — |
| `ukeysort_error` | pass (1/0) | pass (1/0) | agree | — |
| `ukeysort_i` | pass (1/0) | pass (1/0) | agree | — |
| `ukeysort_rand` | pass (1/0) | pass (1/0) | agree | — |
| `umerge` | pass (1/0) | pass (1/0) | agree | — |
| `uniq_1` | pass (1/0) | pass (1/0) | agree | — |
| `uniq_2` | pass (1/0) | pass (1/0) | agree | — |
| `usort_1` | pass (1/0) | pass (1/0) | agree | — |
| `usort_2` | pass (1/0) | pass (1/0) | agree | — |
| `usort_rand` | pass (1/0) | pass (1/0) | agree | — |
| `zip3_fail` | pass (1/0) | pass (1/0) | agree | — |
| `zip3_pad` | pass (1/0) | pass (1/0) | agree | — |
| `zip3_trim` | pass (1/0) | pass (1/0) | agree | — |
| `zip_fail` | pass (1/0) | pass (1/0) | agree | — |
| `zip_pad` | pass (1/0) | pass (1/0) | agree | — |
| `zip_trim` | pass (1/0) | pass (1/0) | agree | — |
| `zip_unzip` | pass (1/0) | pass (1/0) | agree | — |
| `zip_unzip3` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith3` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith3_fail` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith3_pad` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith3_trim` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith_fail` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith_pad` | pass (1/0) | pass (1/0) | agree | — |
| `zipwith_trim` | pass (1/0) | pass (1/0) | agree | — |
| `error_info` | error | fail (0/1) | oracle-only-fail | `unsupported_bif`-class `erlang:system_info(snifs)`; sys side mis-parsed (see §E) |

Totals: cases 78 · sys 77 pass / 0 fail / 1 error · rust 75 pass / 3 fail

### maps_SUITE (31 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `doctests` | pass (1/0) | timeout | rust-gap | `unsupported_bif` `erlang:bump_reductions/1` (doctests) |
| `t_filter_2` | pass (1/0) | timeout | rust-gap | — |
| `t_filtermap_2` | pass (1/0) | timeout | rust-gap | — |
| `t_fold_3` | pass (1/0) | timeout | rust-gap | — |
| `t_foreach_2` | pass (1/0) | timeout | rust-gap | — |
| `t_from_keys` | pass (1/0) | timeout | rust-gap | stacktrace head lacks `{maps,F,A}` frame |
| `t_from_keys_check_trapping` | pass (1/0) | timeout | rust-gap | `erlang:trace/3` → `badarg` |
| `t_from_keys_kill_process` | pass (1/0) | timeout | rust-gap | no yield/preemption in long BIF loop → per-case timeout |
| `t_from_list_check_trapping` | pass (1/0) | timeout | rust-gap | `erlang:trace/3` → `badarg` |
| `t_from_list_kill_process` | pass (1/0) | timeout | rust-gap | no yield/preemption in long BIF loop → per-case timeout |
| `t_get_3` | pass (1/0) | timeout | rust-gap | — |
| `t_groups_from_list` | pass (1/0) | timeout | rust-gap | — |
| `t_intersect` | pass (1/0) | timeout | rust-gap | no yield/preemption in long BIF loop → per-case timeout |
| `t_intersect_with` | pass (1/0) | timeout | rust-gap | no yield/preemption in long BIF loop → per-case timeout |
| `t_iterator_1` | pass (1/0) | timeout | rust-gap | heap-cap → `system_limit` (BIF heap 16 Mi words) |
| `t_iterator_2` | pass (1/0) | timeout | rust-gap | heap-cap → `system_limit` (BIF heap 16 Mi words) |
| `t_iterator_valid` | pass (1/0) | timeout | rust-gap | — |
| `t_keys_kill_process` | pass (1/0) | timeout | rust-gap | no yield/preemption in long BIF loop → per-case timeout |
| `t_keys_trapping` | pass (1/0) | timeout | rust-gap | `erlang:trace/3` → `badarg` |
| `t_map_2` | pass (1/0) | timeout | rust-gap | — |
| `t_merge_opt` | pass (1/0) | timeout | rust-gap | `erts_debug:same/2` false (map fast-path identity) |
| `t_merge_with` | pass (1/0) | timeout | rust-gap | no yield/preemption in long BIF loop → per-case timeout |
| `t_order_consistency` | pass (1/0) | timeout | rust-gap | — |
| `t_put_opt` | pass (1/0) | timeout | rust-gap | `erts_debug:same/2` false (map fast-path identity) |
| `t_size_1` | pass (1/0) | timeout | rust-gap | — |
| `t_update_with_3` | pass (1/0) | timeout | rust-gap | — |
| `t_update_with_4` | pass (1/0) | timeout | rust-gap | — |
| `t_values_kill_process` | pass (1/0) | timeout | rust-gap | no yield/preemption in long BIF loop → per-case timeout |
| `t_values_trapping` | pass (1/0) | timeout | rust-gap | `erlang:trace/3` → `badarg` |
| `t_with_2` | pass (1/0) | timeout | rust-gap | stacktrace head lacks `{maps,F,A}` frame |
| `t_without_2` | pass (1/0) | timeout | rust-gap | stacktrace head lacks `{maps,F,A}` frame |

Totals: cases 31 · sys 31 pass / 0 fail · rust 0 pass / 0 fail / 31 timeout

Every `rust` cell in this table is the **suite-level** kill (seven cases hang past the 330 s suite
budget at a 300 s per-case budget), so `0/0` here is not "31 failures": the same drive at a 5 s
per-case budget answers 10 pass / 21 fail-or-timeout, case by case, in §D7.

### proplists_SUITE (4 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `examples` | pass (1/0) | pass (1/0) | agree | — |
| `map_conversion` | pass (1/0) | pass (1/0) | agree | — |
| `map_conversion_normalize` | pass (1/0) | pass (1/0) | agree | — |
| `pm_fold_test` | pass (1/0) | pass (1/0) | agree | — |

Totals: cases 4 · sys 4 pass / 0 fail · rust 4 pass / 0 fail

### queue_SUITE (6 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `do` | pass (1/0) | pass (1/0) | agree | — |
| `doctests` | pass (1/0) | pass (1/0) | agree | — |
| `error` | pass (1/0) | pass (1/0) | agree | — |
| `io_test` | pass (1/0) | pass (1/0) | agree | — |
| `op_test` | pass (1/0) | pass (1/0) | agree | — |
| `to_list` | pass (1/0) | pass (1/0) | agree | — |

Totals: cases 6 · sys 6 pass / 0 fail · rust 6 pass / 0 fail

### binary_module_SUITE (19 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `ac_simd_root_skip` | pass (1/0) | pass (1/0) | agree | — |
| `badargs` | pass (1/0) | pass (1/0) | agree | — |
| `bin_to_list` | fail (0/1) | fail (0/1) | agree-fail-identical | rust per-case timeout (>300 s); sys exits `badresult` |
| `check_no_invalid_read_bug` | pass (1/0) | pass (1/0) | agree | — |
| `copy` | fail (0/1) | fail (0/1) | agree-fail-identical | identical failure both sides (`mismatch` in the suite) |
| `doctests` | pass (1/0) | fail (0/1) | divergence | `unsupported_bif` `erlang:bump_reductions/1` (doctests) |
| `encode_decode` | fail (0/1) | fail (0/1) | agree-fail-identical | identical failure both sides (`undef binref:encode_unsigned/1` — helper not staged) |
| `guard` | pass (1/0) | error | rust-gap | rust run aborted (`beam_rt: system_limit`, VM-fatal) — no row |
| `hex_encoding` | pass (1/0) | error | rust-gap | rust run aborted (`beam_rt: system_limit`, VM-fatal) — no row |
| `interesting` | fail (0/1) | error | divergence | rust run aborted (VM-fatal `system_limit`); sys case also fails |
| `join` | pass (1/0) | error | rust-gap | rust run aborted (`beam_rt: system_limit`, VM-fatal) — no row |
| `list_to_bin` | fail (0/1) | error | divergence | rust run aborted (VM-fatal `system_limit`); sys case also fails |
| `longest_common_trap` | pass (1/0) | error | rust-gap | rust run aborted (`beam_rt: system_limit`, VM-fatal) — no row |
| `parts` | fail (0/1) | error | divergence | rust run aborted (VM-fatal `system_limit`); sys case also fails |
| `random_ref_comp` | fail (0/1) | error | divergence | rust run aborted (VM-fatal `system_limit`); sys case also fails |
| `random_ref_fla_comp` | fail (0/1) | error | divergence | rust run aborted (VM-fatal `system_limit`); sys case also fails |
| `random_ref_sr_comp` | fail (0/1) | error | divergence | rust run aborted (VM-fatal `system_limit`); sys case also fails |
| `referenced` | pass (1/0) | error | rust-gap | rust run aborted (`beam_rt: system_limit`, VM-fatal) — no row |
| `scope_return` | pass (1/0) | error | rust-gap | rust run aborted (`beam_rt: system_limit`, VM-fatal) — no row |

Totals: cases 19 · sys 10 pass / 9 fail · rust 3 pass / 4 fail / 12 error

### string_SUITE (46 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `casefold` | pass (1/0) | pass (1/0) | agree | — |
| `cd_gc` | pass (1/0) | pass (1/0) | agree | — |
| `centre` | pass (1/0) | pass (1/0) | agree | — |
| `chars` | pass (1/0) | pass (1/0) | agree | — |
| `chomp` | pass (1/0) | pass (1/0) | agree | — |
| `chr_rchr` | pass (1/0) | pass (1/0) | agree | — |
| `copies` | pass (1/0) | pass (1/0) | agree | — |
| `doctests` | pass (1/0) | pass (1/0) | agree | — |
| `equal` | pass (1/0) | pass (1/0) | agree | — |
| `find` | pass (1/0) | pass (1/0) | agree | — |
| `is_empty` | pass (1/0) | pass (1/0) | agree | — |
| `jaro_similarity` | pass (1/0) | pass (1/0) | agree | — |
| `join` | pass (1/0) | pass (1/0) | agree | — |
| `left_right` | pass (1/0) | pass (1/0) | agree | — |
| `len` | pass (1/0) | pass (1/0) | agree | — |
| `length` | pass (1/0) | pass (1/0) | agree | — |
| `lexemes` | pass (1/0) | pass (1/0) | agree | — |
| `lowercase` | pass (1/0) | pass (1/0) | agree | — |
| `meas` | pass (1/0) | pass (1/0) | agree | — |
| `nth_lexeme` | pass (1/0) | pass (1/0) | agree | — |
| `old_concat` | pass (1/0) | pass (1/0) | agree | — |
| `old_equal` | pass (1/0) | pass (1/0) | agree | — |
| `old_to_float` | pass (1/0) | pass (1/0) | agree | — |
| `old_to_integer` | pass (1/0) | pass (1/0) | agree | — |
| `old_tokens` | pass (1/0) | pass (1/0) | agree | — |
| `pad` | pass (1/0) | pass (1/0) | agree | — |
| `prefix` | pass (1/0) | pass (1/0) | agree | — |
| `replace` | pass (1/0) | pass (1/0) | agree | — |
| `reverse` | pass (1/0) | pass (1/0) | agree | — |
| `slice` | pass (1/0) | pass (1/0) | agree | — |
| `span_cspan` | pass (1/0) | pass (1/0) | agree | — |
| `split` | pass (1/0) | pass (1/0) | agree | — |
| `str_rstr` | pass (1/0) | pass (1/0) | agree | — |
| `strip` | pass (1/0) | pass (1/0) | agree | — |
| `sub_string` | pass (1/0) | pass (1/0) | agree | — |
| `sub_word` | pass (1/0) | pass (1/0) | agree | — |
| `substr` | pass (1/0) | pass (1/0) | agree | — |
| `take` | pass (1/0) | pass (1/0) | agree | — |
| `titlecase` | pass (1/0) | pass (1/0) | agree | — |
| `to_float` | pass (1/0) | pass (1/0) | agree | — |
| `to_graphemes` | pass (1/0) | pass (1/0) | agree | — |
| `to_integer` | pass (1/0) | pass (1/0) | agree | — |
| `to_upper_to_lower` | pass (1/0) | pass (1/0) | agree | — |
| `trim` | pass (1/0) | pass (1/0) | agree | — |
| `uppercase` | pass (1/0) | pass (1/0) | agree | — |
| `words` | pass (1/0) | pass (1/0) | agree | — |

Totals: cases 46 · sys 46 pass / 0 fail · rust 46 pass / 0 fail

### unicode_SUITE (19 cases)

| case | sys | rust | verdict | root-cause class |
| --- | --- | --- | --- | --- |
| `bin_is_7bit` | pass (1/0) | pass (1/0) | agree | — |
| `binaries_errors_limit` | pass (1/0) | fail (0/1) | divergence | `unsupported_bif` `erts_debug:set_internal_state(unicode_loop_limit,N)` |
| `category` | pass (1/0) | fail (0/1) | divergence | heap-cap → `system_limit` (BIF heap 16 Mi words) |
| `ex_binaries_errors_utf16_big` | pass (1/0) | fail (0/1) | divergence | `binary:referenced_byte_size/1` (own size, not referenced) |
| `ex_binaries_errors_utf16_little` | pass (1/0) | fail (0/1) | divergence | `binary:referenced_byte_size/1` (own size, not referenced) |
| `ex_binaries_errors_utf32_big` | pass (1/0) | fail (0/1) | divergence | `binary:referenced_byte_size/1` (own size, not referenced) |
| `ex_binaries_errors_utf32_little` | pass (1/0) | fail (0/1) | divergence | `binary:referenced_byte_size/1` (own size, not referenced) |
| `ex_binaries_errors_utf8` | pass (1/0) | fail (0/1) | divergence | `binary:referenced_byte_size/1` (own size, not referenced) |
| `exceptions` | pass (1/0) | fail (0/1) | divergence | `unsupported_bif` `erts_debug:set_internal_state(unicode_loop_limit,N)` |
| `huge_illegal_code_points` | pass (1/0) | pass (1/0) | agree | — |
| `incomplete_utf32_to_latin1` | pass (1/0) | pass (1/0) | agree | — |
| `is_id` | fail (0/1) | fail (0/1) | agree-fail-identical | identical: `{badmatch,{error,enoent}}` (UnicodeData.txt data-dir absent on both) |
| `is_whitespace` | fail (0/1) | fail (0/1) | agree-fail-identical | identical: `{badmatch,{error,enoent}}` (UnicodeData.txt data-dir absent on both) |
| `latin1` | pass (1/0) | fail (0/1) | divergence | `unsupported_bif` `erts_debug:set_internal_state(unicode_loop_limit,N)` |
| `normalize` | pass (1/0) | pass (1/0) | agree | — |
| `random_lists` | fail (0/1) | fail (0/1) | agree-fail-identical | BOTH fail, different reasons (sys `undef random_iolist`; rust `unsupported_bif set_internal_state`) |
| `roundtrips` | pass (1/0) | fail (0/1) | divergence | `unsupported_bif` `erts_debug:set_internal_state(unicode_loop_limit,N)` |
| `utf16_illegal_sequences_bif` | pass (1/0) | fail (0/1) | divergence | `unsupported_bif` `erts_debug:set_internal_state(unicode_loop_limit,N)` |
| `utf8_illegal_sequences_bif` | pass (1/0) | fail (0/1) | divergence | `unsupported_bif` `erts_debug:set_internal_state(unicode_loop_limit,N)` |

Totals: cases 19 · sys 16 pass / 3 fail · rust 4 pass / 15 fail

## C. Totals

| suite | cases | sys pass | sys fail | sys other | rust pass | rust fail | rust other | agree | agree-fail-identical | divergence | rust-gap | oracle-only-fail |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| dict_SUITE | 7 | 7 | 0 | 0 | 7 | 0 | 0 | 7 | 0 | 0 | 0 | 0 |
| lists_SUITE | 78 | 77 | 0 | 1 | 75 | 3 | 0 | 75 | 0 | 2 | 0 | 1 |
| maps_SUITE | 31 | 31 | 0 | 0 | 0 | 0 | 31 | 0 | 0 | 0 | 31 | 0 |
| proplists_SUITE | 4 | 4 | 0 | 0 | 4 | 0 | 0 | 4 | 0 | 0 | 0 | 0 |
| queue_SUITE | 6 | 6 | 0 | 0 | 6 | 0 | 0 | 6 | 0 | 0 | 0 | 0 |
| binary_module_SUITE | 19 | 10 | 9 | 0 | 3 | 4 | 12 | 3 | 3 | 7 | 6 | 0 |
| string_SUITE | 46 | 46 | 0 | 0 | 46 | 0 | 0 | 46 | 0 | 0 | 0 | 0 |
| unicode_SUITE | 19 | 16 | 3 | 0 | 4 | 15 | 0 | 4 | 3 | 12 | 0 | 0 |
| **total** | **210** | **197** | **12** | **1** | **145** | **22** | **43** | **145** | **6** | **21** | **37** | **1** |

Counts, not rates: the two sides do not run the same number of cases per suite (the Rust side
loses rows when its process dies).

Reading of the numbers: **145 of 210 cases (69%) agree**; **6** are excluded as
`agree-fail-identical` (the SAME failure on both sides — excluded from both numerators, never
counted as passes); **21** divergences and **37** rust-gaps plus **1** oracle-only-fail remain
and are itemised in §D.
## D. Divergences and rust-gaps, by root-cause class

Every reduction below was run under **both** runtimes at
`erts_rust/target/release/beam_rt -noshell -eval …` versus `erl -noshell -eval …` (or, where
noted, through a *compiled* probe module on the same `-pa` set), and the two answers are quoted
verbatim. Nothing here was fixed — these are observations for the owning crates.

### D1. BIF-heap budget → `error:system_limit` where the oracle completes (5 rows + 12 fallout rows)

Smallest failing expression (compiled probe module `cap.erl`, `-pa` on both sides):

```erlang
cap:dup(N) -> length(lists:duplicate(N, gurka)).
%%  cap:dup(1048596)  oracle 1048596   rust 1048596      (fits)
%%  cap:dup(10000000) oracle 10000000  rust {'EXIT',{system_limit,[]}}
cap:seq(N) -> length(lists:seq(1, N)).          % same shape for lists:seq/2
map_size(maps:from_keys(lists:seq(1,1000000), ok))
%%  oracle 1000000     rust {'EXIT',{system_limit,[]}}
```

The runtime's BIF heap region is a fixed arena, `beam_rt::config::DEFAULT_HEAP_WORDS = 1 << 24`
words (128 MB); a **single** BIF that would build a term past the remaining budget of that region
raises `system_limit`, where the oracle grows/YIELDs and succeeds. The same terms built by
*Erlang* code (a recursive `[x|A]` loop to 8 M elements) are fine, so the ceiling is the BIF
region, not the process heap.

It is **state-dependent** in-suite: `cap:loop()` (the exact `append_trapping_1/1` shape of
`lists_SUITE:append_2`, N = 0…20) passes standalone on both sides, yet `lists_SUITE:append_2`
fails in the suite (both sweep runs, at loadavg ≈ 117 and at loadavg ≈ 1.2) because earlier cases
have already retained heap. Caveat on the reduction vehicle: going through `-eval` does **not**
reproduce it, because the `-eval` evaluator does not build on the BIF heap region — the compiled
module above is the valid form.

Rows: `lists_SUITE:append_2`, `lists_SUITE:subtract`, `unicode_SUITE:category`
(`lists:seq(1,16#10FFFF)`), `maps_SUITE:t_iterator_1`, `maps_SUITE:t_iterator_2`
(`maps:from_list/1`); plus the `binary_module_SUITE` aborts of D9.

### D2. `erlang:trace/3` rejects `[running, {tracer, Pid}]` → `badarg` (4 rows)

```erlang
erlang:trace(self(), true, [running, {tracer, self()}]).
%%  oracle 1     rust {'EXIT',{badarg,[…]}}
```

Rows: `maps_SUITE:t_keys_trapping`, `t_values_trapping`, `t_from_keys_check_trapping`,
`t_from_list_check_trapping` — all fail in the suite's `count_nr_of_yields/2` helper with
`{badmatch,badarg}`. The suite's other traceless cases are unaffected.

### D3. `erts_debug:set_internal_state(unicode_loop_limit, N)` → `unsupported_bif` (6 rows)

```erlang
A = erts_debug:set_internal_state(available_internal_state, true),
erts_debug:set_internal_state(unicode_loop_limit, 10).
%%  oracle {false,40000}     rust {false,{'EXIT',{unsupported_bif,…}}}
```

`available_internal_state` **is** served (both answer `false`); the per-verb setter is the typed
capability gap. Rows: `unicode_SUITE:binaries_errors_limit`, `exceptions`, `latin1`, `roundtrips`,
`utf16_illegal_sequences_bif`, `utf8_illegal_sequences_bif`. (`binary_module_SUITE` uses the
sibling verb `binary_loop_limit`, which is equally missing — those cases are killed earlier by D9.)

### D4. `binary:referenced_byte_size/1` on a sub-binary reports its own size (5 rows)

```erlang
Partly  = binary:copy(<<OK/binary, (binary:copy(<<255>>, 1024))/binary>>),   % 1279 bytes
Chomped = binary:part(Partly, 769, 510),
{byte_size(Chomped), binary:referenced_byte_size(Chomped)}.
%%  oracle {510,1279}     rust {510,510}
```

Rows: `unicode_SUITE:ex_binaries_errors_utf8`, `ex_binaries_errors_utf16_big`,
`ex_binaries_errors_utf16_little`, `ex_binaries_errors_utf32_big`,
`ex_binaries_errors_utf32_little` — each dies on the inner loop's assertion
`true = (binary:referenced_byte_size(Chomped) =:= PBSz)` with `{badmatch,false}`.

### D5. `erts_debug:same/2` is false for the map fast-path identity (2 rows)

```erlang
M = (fun(X) -> X end)(#{a => 1}),          % defeat constant folding
erts_debug:same(maps:merge(#{}, M), M).
%%  oracle true      rust false
%% same for maps:merge(M, M), maps:merge(M, #{}), and maps:put(K,V,M) with an unchanged value
%% (erts_debug:same(M, M) alone IS true on both)
```

The oracle returns the *input map object* from `maps:merge/2`/`maps:put/3` when nothing changes;
this runtime returns a copy. Rows: `maps_SUITE:t_merge_opt`, `maps_SUITE:t_put_opt`
(`{badmatch,false}`).

### D6. The raised term's stacktrace head lacks the raising `{maps,F,A}` frame (3 rows)

`maps_SUITE.erl:53` defines `-define(badmap(V,F,Args), {'EXIT', {{badmap,V}, [{maps,F,Args,_}|_]}})`,
i.e. the macro matches the **head** of the stacktrace. Both runtimes produce the same reason:

```erlang
catch maps:with([a,b], (fun(X) -> X end)(a)).
%%  oracle {'EXIT',{{badmap,a},[{maps,with,[[a,b],a],…},{erl_eval,…}|…]}}
%%  rust   {'EXIT',{{badmap,a},[{maps_SUITE,t_with_2,1,…},…]}}      % caller frame first
```

Rows: `maps_SUITE:t_with_2` (line 390), `t_without_2`, `t_from_keys` — each fails as
`{badmatch,{'EXIT',{{badmap…}}}}` because the innermost frame is missing.

### D7. Long BIF loops neither yield nor pre-empt → per-case `TIMEOUT` (7 rows)

Rows: `maps_SUITE:t_from_keys_kill_process`, `t_from_list_kill_process`, `t_keys_kill_process`,
`t_values_kill_process`, `t_intersect`, `t_intersect_with`, `t_merge_with`. Their shape is a
`maps:from_keys/2` / `maps:merge/2` / `maps:intersect/2` over 10 000–1 000 000 keys inside a
`spawn_monitor`ed loop that a killer process is supposed to interrupt (`exit(Pid,kill)` between
rounds, or the round loop itself never terminating). Because the BIF loop never reaches a
scheduling point, the kill never lands and the case burns the whole per-case budget. This is the
**observed symptom** of the same region whose hard ceiling is D1.

For evidence at a finer grain than the canonical artifact (which records only
`maps_SUITE … suite killed after 330017 ms` for all 31 rows, because 7 hung cases × 300 s blow the
suite budget), `results/stdlib/raw/maps-{rust,sys}-per-case-5s.txt` hold the identical drive run
with a **5 s** per-case budget:

| case | sys (5 s budget) | rust (5 s budget) |
| --- | --- | --- |
| doctests | OK | EXC `{1,errors}` (ct_doctest → D8) |
| t_filter_2, t_filtermap_2, t_fold_3, t_foreach_2, t_get_3, t_groups_from_list, t_iterator_valid, t_map_2, t_order_consistency, t_size_1, t_update_with_3, t_update_with_4 | OK | OK |
| t_from_keys, t_with_2, t_without_2 | OK | EXC (D6) |
| t_merge_opt, t_put_opt | OK | EXC `{badmatch,false}` (D5) |
| t_keys_trapping, t_values_trapping, t_from_keys_check_trapping, t_from_list_check_trapping | OK | EXC `{badmatch,badarg}` (D2) |
| t_iterator_1, t_iterator_2 | OK | EXC `system_limit` (D1) |
| t_from_keys_kill_process, t_from_list_kill_process, t_keys_kill_process, t_values_kill_process, t_intersect, t_intersect_with, t_merge_with | OK | TIMEOUT (D7) |

So the Rust side of `maps_SUITE` is 10/31 pass at a 5 s budget, not 0/31: the canonical 0/31 is a
**reporting consequence** of the seven hangs, not 31 separate failures.

### D8. `erlang:bump_reductions/1` is not served (2 rows)

```erlang
erlang:bump_reductions(134217727).
%%  oracle true      rust the doctest's own error  ('unsupported: erlang:bump_reductions/1')
```

Reached from `prim_eval:arg_reg_alloc/0` inside the `ct_doctest` machinery. Rows:
`binary_module_SUITE:doctests` and `maps_SUITE:doctests` (both `{1,errors}`).

### D9. A VM-fatal `system_limit` aborts the whole `beam_rt` process mid-suite (12 rows)

The Rust leg of `binary_module_SUITE` stops emitting `CASE` lines after `encode_decode`; the
process then exits with

```
beam_rt: system_limit
  in call from ct_runner:'-run_case/4-fun-0-'/3 (…/ctl/ct_runner.erl:152)
```

(`results/stdlib/raw/binary_module-rust-tail.txt`, reproduced twice). The raise happens while
running `guard/1`, a case whose entire body is `{comment, "Guard tests are run in emulator test
suite"}` — which **passes** when run alone (`probe:run_case(binary_module_SUITE, guard)` → the
comment term). So the abort is not `guard`'s logic: it is a `system_limit` that escapes a
`spawn_monitor`ed worker and terminates the VM instead of being delivered as a `DOWN` message
(the allocation-pressure symptom of D1 — `bin_to_list/1` is killed at its 300 s per-case timeout
immediately before, after building ~10⁶-element lists in a random loop).

Consequence for the report: the six cases after it (`guard`, `hex_encoding`, `join`,
`longest_common_trap`, `referenced`, `scope_return`) are recorded `rust-gap / case missing from
the rust runtime's output`, and the six that the *oracle* also fails (`interesting`, `list_to_bin`,
`parts`, `random_ref_comp`, `random_ref_fla_comp`, `random_ref_sr_comp`) are recorded `divergence`
for the same reason — the Rust row is missing, so the harness cannot call them identical. Their
oracle-side failures are all the suite's own `?MASK_ERROR`-style mismatch (e.g.
`{exception,exit,mismatch,{binary_module_SUITE,random_copy,1,…}}` and `undef random_iolist:run`
for an unstaged helper).

### D10. `erlang:system_info(snifs)` is not published (1 row, plus an oracle-side parse artifact)

```erlang
erlang:system_info(snifs).
%%  oracle a list of NIF rows     rust 'unsupported: erlang:system_info(snifs): this runtime
%%                                     publishes no static NIF table'
```

Row: `lists_SUITE:error_info`, recorded `oracle-only-fail` because the **oracle's own** row is
missing from the parsed output. That is a line-protocol artifact, not a divergence: the case
prints a diagnostic without a trailing newline and the runner's `CASE error_info OK` is glued onto
it (`results/stdlib/raw/lists-sys-stdout.txt`: 78 `CASE ` occurrences, 77 at line start; the
glued one reads `…ct_runner.erl:152)CASE error_info OK`). The Rust leg of the same case genuinely
fails on `system_info(snifs)`.

## E. Exclusions — identical-failure rows (6), and what is NOT a pass

These are **excluded from both numerators**, never counted as passes. Each was confirmed by
reading the `detail` of *both* sides in `report.json` (the raw reason strings are quoted below).

| case | sys reason | rust reason | why identical |
| --- | --- | --- | --- |
| `binary_module_SUITE:copy` | `{exception,exit,mismatch,{binary_module_SUITE,random_copy,1,_}}` | same | the suite's own randomized comparison fails identically (its random generator is the same code on both sides) |
| `binary_module_SUITE:encode_decode` | `{exception,error,undef,{binref,encode_unsigned,[…]}}` | same | the helper module `binref` is not among the staged helper modules (`HELPER_MODULES` = `error_info_lib`, `dict_test_lib`), so `undef` on both sides |
| `unicode_SUITE:is_id` | `{badmatch,{error,enoent}}` in `parse_properties/1` | same | needs `UnicodeData.txt` from a `ct` data dir; the runner passes the stage's source dir as `data_dir`, so the file is absent on both sides |
| `unicode_SUITE:is_whitespace` | `{badmatch,{error,enoent}}` | same | same data-dir dependency |
| `unicode_SUITE:random_lists` | `{exception,error,undef,{random_iolist,run,…}}` | `{exception,error,unsupported_bif,{erts_debug,set_internal_state,…}}` | **both fail, but not for the same reason** — kept in the excluded bucket because both sides failed, flagged here explicitly: the oracle dies on the unstaged `random_iolist` helper before it reaches the `erts_debug` call that the Rust side cannot serve (D3) |
| `binary_module_SUITE:bin_to_list` | `{exception,exit,badresult,…}` | per-case `TIMEOUT` (300 s) | **also flagged**: both sides "fail", but the oracle fails fast inside the case while the Rust side never finishes; this row is the Rust side of D1/D9 |

Two further non-pass, non-fail statuses worth naming:

* `maps_SUITE` — all 31 rows are `rust-gap / suite killed after 330017 ms`; the honest per-case
  picture is the 5 s-budget table in D7 (10 pass / 21 fail-or-timeout).
* `string_SUITE` — 46 cases, **all** `agree`.

## F. Caveat (read before quoting any number)

The host is shared (16 logical CPUs; loadavg at the two sweep runs was `116.82 …` → `3.58 …` and
`1.17 …` → `1.56 …`), so only the **counts** in this document are portable: a wall time, a
timeout verdict and the exact threshold at which the D1 ceiling bites all move with machine load
and with how much heap earlier cases in the same VM retained. The runner also deliberately does
not execute `init_per_group/1`, `end_per_group/1`, `init_per_testcase/2` or `end_per_testcase/2`,
and it passes the staged source directory as both `data_dir` and `priv_dir`; any case that needs
a real `ct` data/privacy directory, a group hook, or a helper module that `HELPER_MODULES` does
not stage fails **identically** on both sides and lands in §E rather than in the pass column.
Where a case's own output is not newline-terminated the harness's line scan can also lose a
`CASE` line (§D10). Nothing in §B/§C should be read as "the suite passes on the Rust runtime";
the honest summary is **145 of 210 measured cases agree, 6 are excluded as identical failures,
21 diverge, 37 are rust-gaps and 1 is an oracle-side parse artifact** — with `queue_SUITE` (6/6),
`proplists_SUITE` (4/4), `dict_SUITE` (7/7) and `string_SUITE` (46/46) fully agreeing, `lists_SUITE`
75/78, `unicode_SUITE` 4/19, `binary_module_SUITE` 3/7 measured and `maps_SUITE` 0/31 as recorded.

## G. Harness change made during this sweep (one line, in `data/ct_runner.erl`)

`expand_group/3` only matched the 3-tuple group form `{Name, Props, Cases}`, so a group declared in
`ct`'s default-properties form `{Name, Cases}` (e.g. every group in `string_SUITE:groups/0`) was
treated as a *case name* and reported as `case not exported`. The first sweep therefore measured
3 rows for `string_SUITE` (2 × "case not exported" + `doctests`). The fix adds the `{Group, Sub}
when is_list(Sub)` clause next to the existing one; `string_SUITE` then expands to its real
**46 cases — all 46 agree** on both sides. No other part of `src/` was touched, and no runtime
crate was modified.
