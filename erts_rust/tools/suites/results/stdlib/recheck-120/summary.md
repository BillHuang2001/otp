# Test-suite parity sweep

Generated at `1793388339` (Unix seconds) by `suites run --target stdlib --suite maps_SUITE,lists_SUITE,binary_module_SUITE --timeout 120 --suite-budget 900 --jobs 1 --out results/stdlib/recheck-120`.

## Environment

- oracle: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — `29/17.1.1`
- rust runtime: `/home/bill/Source/otp/.genesis/workers/worker_T1_A18/erts_rust/target/release/beam_rt` — `30/17.1`
- rust workspace commit: `8e8e0ad5d96a11931ff6972779a3b5adbe90bfca`
- tier: `default`
- loadavg before/after: `4.26 4.58 5.30 4/867 3` / `3.93 4.39 4.98 3/872 596` on 16 logical CPUs
- the host is **shared**: only pass/fail counts are portable, never a wall time

### Target `stdlib` (ct)

- source: `/home/bill/Source/otp/.genesis/workers/worker_T1_A18/lib/stdlib/test` (otp) at `bf0386a797c0`
- staged copy: `/tmp/erts-suites-stage/stdlib/src`
- build: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erlc -o <stage>/ebin -I <stage>/src <suite>.erl …` — ok (reused a completed build)
- build detail: compiled 3 suites + 6 helpers + runner with the oracle's erlc
- tally: measured 130 · agree 117 · agree-fail-identical (excluded) 1 · divergence 12 · oracle-only-fail 0 · rust-gap 0 · not-run 0
- totals: sys 129/1 passed/failed · rust 117/13 passed/failed

Notes:
- drive (identical on both sides): `ct_runner:run('{suite}', {case_timeout_ms}, {suite_budget_ms}, "{data_root}"), halt(0).`
- each case runs in its own `spawn_monitor` with a per-case timeout; `init_per_group`/`end_per_group`/`init_per_testcase` hooks are NOT executed
- cases that fail identically on both sides (missing `ct` config/data dirs, group-hook dependencies) are `agree-fail-identical`: excluded from both numerators, never counted as passes
- per-case timeout: 120 s · suite budget: 900 s (the runner stops launching cases when the budget expires and prints `SUITE_DEADLINE <not-run>`, so the remaining cases are reported as *not measured*, never as failures)
- binary_module_SUITE: 20 cases — sys 19/1 pass/fail, rust 12/8 pass/fail
- lists_SUITE: 78 cases — sys 78/0 pass/fail, rust 78/0 pass/fail
- maps_SUITE: 32 cases — sys 32/0 pass/fail, rust 27/5 pass/fail

| row | sys (P/F) | rust (P/F) | verdict | first divergence / note |
|---|---|---|---|---|
| `binary_module_SUITE:ac_simd_root_skip` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:badargs` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:bin_to_list` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:check_no_invalid_read_bug` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:copy` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:doctests` | 1/0 | 0/1 | divergence | {exception,error,{1,errors},{ct_doctest,run_module_docs,3,...}} |
| `binary_module_SUITE:encode_decode` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:error_info` | 1/0 | 0/1 | divergence | 'exit <structured reason>' |
| `binary_module_SUITE:guard` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:hex_encoding` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:interesting` | 0/1 | 0/1 | agree-fail-identical | {exception,error,{badmatch,false},{binary_module_SUITE,do_interesting,1,...}} |
| `binary_module_SUITE:join` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:list_to_bin` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:longest_common_trap` | 1/0 | 0/1 | divergence | {exception,error,unsupported_bif,{erlang,bump_reductions,[...],...}} |
| `binary_module_SUITE:parts` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{binary_module_SUITE,'-random_parts/1-lc$^1/1-1-',2,...}} |
| `binary_module_SUITE:random_ref_comp` | 1/0 | 0/1 | divergence | timeout |
| `binary_module_SUITE:random_ref_fla_comp` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:random_ref_sr_comp` | 1/0 | 0/1 | divergence | {exception,exit,mismatch,{binary_module_SUITE,do_replace_comp,4,...}} |
| `binary_module_SUITE:referenced` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,1023},{binary_module_SUITE,referenced,1,...}} |
| `binary_module_SUITE:scope_return` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:append_1` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:append_2` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:doctests` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:droplast` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:dropwhile` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:enumerate` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:error_info` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:filter_partition` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:flatten_1` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:flatten_1_e` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:flatten_2` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:flatten_2_e` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:hof` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:join` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keymember` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keymerge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keyreplace` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keysearch_keyfind` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keysort_1` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keysort_error` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keysort_i` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keysort_rand` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keystore` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:keytake` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:member` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:merge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:otp_5939` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:otp_6023` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:otp_6606` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:otp_7230` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:reverse` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:rkeymerge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:rmerge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:rukeymerge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:rumerge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:search` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:seq_2` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:seq_2_e` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:seq_3` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:seq_3_e` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:seq_loop` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:sort_1` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:sort_2` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:sort_rand` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:sublist_2` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:sublist_2_e` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:sublist_3` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:sublist_3_e` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:subtract` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:suffix` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:takewhile` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:ukeymerge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:ukeysort_1` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:ukeysort_error` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:ukeysort_i` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:ukeysort_rand` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:umerge` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:uniq_1` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:uniq_2` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:usort_1` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:usort_2` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:usort_rand` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip3_fail` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip3_pad` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip3_trim` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip_fail` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip_pad` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip_trim` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip_unzip` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zip_unzip3` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith3` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith3_fail` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith3_pad` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith3_trim` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith_fail` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith_pad` | 1/0 | 1/0 | agree |  |
| `lists_SUITE:zipwith_trim` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:doctests` | 1/0 | 0/1 | divergence | {exception,error,{1,errors},{ct_doctest,run_module_docs,3,...}} |
| `maps_SUITE:error_info` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_filter_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_filtermap_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_fold_3` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_foreach_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_from_keys` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_from_keys_check_trapping` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,system_limit},{maps_SUITE,count_nr_of_yields,2,...}} |
| `maps_SUITE:t_from_keys_kill_process` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_from_list_check_trapping` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{maps_SUITE,t_from_list_check_trapping,1,...}} |
| `maps_SUITE:t_from_list_kill_process` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_get_3` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_groups_from_list` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_intersect` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_intersect_with` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_iterator_1` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_iterator_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_iterator_valid` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_keys_kill_process` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_keys_trapping` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,system_limit},{maps_SUITE,count_nr_of_yields,2,...}} |
| `maps_SUITE:t_map_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_merge_opt` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_merge_with` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_order_consistency` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_put_opt` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_size_1` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_update_with_3` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_update_with_4` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_values_kill_process` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_values_trapping` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,system_limit},{maps_SUITE,count_nr_of_yields,2,...}} |
| `maps_SUITE:t_with_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_without_2` | 1/0 | 1/0 | agree |  |

Recorded invocations (the two sides get the *same* `-pa` set and the *same* `-eval`):

```
# ct binary_module_SUITE [sys]
cd /tmp/erts-suites-stage/stdlib/cwd/binary_module_SUITE && /nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl -noshell -pa /tmp/erts-suites-stage/stdlib/ebin -pa /tmp/erts-suites-stage/stdlib/ctl -eval 'ct_runner:run('\''binary_module_SUITE'\'', 120000, 900000, "/tmp/erts-suites-stage/stdlib/src"), halt(0).'
# ct binary_module_SUITE [rust]
cd /tmp/erts-suites-stage/stdlib/cwd/binary_module_SUITE && /home/bill/Source/otp/.genesis/workers/worker_T1_A18/erts_rust/target/release/beam_rt -noshell -pa /tmp/erts-suites-stage/stdlib/ebin -pa /tmp/erts-suites-stage/stdlib/ctl -eval 'ct_runner:run('\''binary_module_SUITE'\'', 120000, 900000, "/tmp/erts-suites-stage/stdlib/src"), halt(0).'
# erlc [build]
/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erlc -o <stage>/ebin -I <stage>/src <suite>.erl …
```

