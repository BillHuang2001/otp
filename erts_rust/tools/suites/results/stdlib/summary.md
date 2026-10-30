# Test-suite parity sweep

Generated at `1793387656` (Unix seconds) by `suites run --target stdlib --suite lists_SUITE,maps_SUITE,proplists_SUITE,dict_SUITE,queue_SUITE,binary_module_SUITE,string_SUITE,unicode_SUITE --timeout 10 --suite-budget 300 --jobs 4 --out results/stdlib`.

## Environment

- oracle: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — `29/17.1.1`
- rust runtime: `/home/bill/Source/otp/.genesis/workers/worker_T1_A18/erts_rust/target/release/beam_rt` — `30/17.1`
- rust workspace commit: `8e8e0ad5d96a11931ff6972779a3b5adbe90bfca`
- tier: `default`
- loadavg before/after: `4.63 5.47 5.82 13/900 3` / `5.30 5.59 5.83 2/848 1099` on 16 logical CPUs
- the host is **shared**: only pass/fail counts are portable, never a wall time

### Target `stdlib` (ct)

- source: `/home/bill/Source/otp/.genesis/workers/worker_T1_A18/lib/stdlib/test` (otp) at `8e8e0ad5d96a`
- staged copy: `/tmp/erts-suites-stage/stdlib/src`
- build: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erlc -o <stage>/ebin -I <stage>/src <suite>.erl …` — ok (reused a completed build)
- build detail: compiled 8 suites + 6 helpers + runner with the oracle's erlc
- tally: measured 214 · agree 183 · agree-fail-identical (excluded) 5 · divergence 26 · oracle-only-fail 0 · rust-gap 0 · not-run 0
- totals: sys 209/5 passed/failed · rust 183/31 passed/failed

Notes:
- drive (identical on both sides): `ct_runner:run('{suite}', {case_timeout_ms}, {suite_budget_ms}, "{data_root}"), halt(0).`
- each case runs in its own `spawn_monitor` with a per-case timeout; `init_per_group`/`end_per_group`/`init_per_testcase` hooks are NOT executed
- cases that fail identically on both sides (missing `ct` config/data dirs, group-hook dependencies) are `agree-fail-identical`: excluded from both numerators, never counted as passes
- per-case timeout: 10 s · suite budget: 300 s (the runner stops launching cases when the budget expires and prints `SUITE_DEADLINE <not-run>`, so the remaining cases are reported as *not measured*, never as failures)
- binary_module_SUITE: 20 cases — sys 18/2 pass/fail, rust 9/11 pass/fail
- dict_SUITE: 7 cases — sys 7/0 pass/fail, rust 7/0 pass/fail
- lists_SUITE: 78 cases — sys 78/0 pass/fail, rust 77/1 pass/fail
- maps_SUITE: 32 cases — sys 32/0 pass/fail, rust 25/7 pass/fail
- proplists_SUITE: 4 cases — sys 4/0 pass/fail, rust 4/0 pass/fail
- queue_SUITE: 7 cases — sys 7/0 pass/fail, rust 7/0 pass/fail
- string_SUITE: 46 cases — sys 45/1 pass/fail, rust 45/1 pass/fail
- unicode_SUITE: 20 cases — sys 18/2 pass/fail, rust 9/11 pass/fail

| row | sys (P/F) | rust (P/F) | verdict | first divergence / note |
|---|---|---|---|---|
| `binary_module_SUITE:ac_simd_root_skip` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:badargs` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:bin_to_list` | 1/0 | 0/1 | divergence | timeout |
| `binary_module_SUITE:check_no_invalid_read_bug` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:copy` | 0/1 | 0/1 | agree-fail-identical | timeout |
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
| `binary_module_SUITE:random_ref_fla_comp` | 1/0 | 0/1 | divergence | timeout |
| `binary_module_SUITE:random_ref_sr_comp` | 1/0 | 0/1 | divergence | timeout |
| `binary_module_SUITE:referenced` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,1023},{binary_module_SUITE,referenced,1,...}} |
| `binary_module_SUITE:scope_return` | 1/0 | 1/0 | agree |  |
| `dict_SUITE:create` | 1/0 | 1/0 | agree |  |
| `dict_SUITE:doctests_dict` | 1/0 | 1/0 | agree |  |
| `dict_SUITE:doctests_gb_trees` | 1/0 | 1/0 | agree |  |
| `dict_SUITE:doctests_orddict` | 1/0 | 1/0 | agree |  |
| `dict_SUITE:iterate` | 1/0 | 1/0 | agree |  |
| `dict_SUITE:remove` | 1/0 | 1/0 | agree |  |
| `dict_SUITE:store` | 1/0 | 1/0 | agree |  |
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
| `lists_SUITE:suffix` | 1/0 | 0/1 | divergence | timeout |
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
| `maps_SUITE:t_iterator_2` | 1/0 | 0/1 | divergence | timeout |
| `maps_SUITE:t_iterator_valid` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_keys_kill_process` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_keys_trapping` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,system_limit},{maps_SUITE,count_nr_of_yields,2,...}} |
| `maps_SUITE:t_map_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_merge_opt` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_merge_with` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_order_consistency` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_put_opt` | 1/0 | 0/1 | divergence | timeout |
| `maps_SUITE:t_size_1` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_update_with_3` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_update_with_4` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_values_kill_process` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_values_trapping` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,system_limit},{maps_SUITE,count_nr_of_yields,2,...}} |
| `maps_SUITE:t_with_2` | 1/0 | 1/0 | agree |  |
| `maps_SUITE:t_without_2` | 1/0 | 1/0 | agree |  |
| `proplists_SUITE:examples` | 1/0 | 1/0 | agree |  |
| `proplists_SUITE:map_conversion` | 1/0 | 1/0 | agree |  |
| `proplists_SUITE:map_conversion_normalize` | 1/0 | 1/0 | agree |  |
| `proplists_SUITE:pm_fold_test` | 1/0 | 1/0 | agree |  |
| `queue_SUITE:do` | 1/0 | 1/0 | agree |  |
| `queue_SUITE:doctests` | 1/0 | 1/0 | agree |  |
| `queue_SUITE:error` | 1/0 | 1/0 | agree |  |
| `queue_SUITE:io_test` | 1/0 | 1/0 | agree |  |
| `queue_SUITE:oops` | 1/0 | 1/0 | agree |  |
| `queue_SUITE:op_test` | 1/0 | 1/0 | agree |  |
| `queue_SUITE:to_list` | 1/0 | 1/0 | agree |  |
| `string_SUITE:casefold` | 1/0 | 1/0 | agree |  |
| `string_SUITE:cd_gc` | 1/0 | 1/0 | agree |  |
| `string_SUITE:centre` | 1/0 | 1/0 | agree |  |
| `string_SUITE:chars` | 1/0 | 1/0 | agree |  |
| `string_SUITE:chomp` | 1/0 | 1/0 | agree |  |
| `string_SUITE:chr_rchr` | 1/0 | 1/0 | agree |  |
| `string_SUITE:copies` | 1/0 | 1/0 | agree |  |
| `string_SUITE:doctests` | 1/0 | 1/0 | agree |  |
| `string_SUITE:equal` | 1/0 | 1/0 | agree |  |
| `string_SUITE:find` | 1/0 | 1/0 | agree |  |
| `string_SUITE:is_empty` | 1/0 | 1/0 | agree |  |
| `string_SUITE:jaro_similarity` | 1/0 | 1/0 | agree |  |
| `string_SUITE:join` | 1/0 | 1/0 | agree |  |
| `string_SUITE:left_right` | 1/0 | 1/0 | agree |  |
| `string_SUITE:len` | 1/0 | 1/0 | agree |  |
| `string_SUITE:length` | 1/0 | 1/0 | agree |  |
| `string_SUITE:lexemes` | 1/0 | 1/0 | agree |  |
| `string_SUITE:lowercase` | 1/0 | 1/0 | agree |  |
| `string_SUITE:meas` | 0/1 | 0/1 | agree-fail-identical | timeout |
| `string_SUITE:nth_lexeme` | 1/0 | 1/0 | agree |  |
| `string_SUITE:old_concat` | 1/0 | 1/0 | agree |  |
| `string_SUITE:old_equal` | 1/0 | 1/0 | agree |  |
| `string_SUITE:old_to_float` | 1/0 | 1/0 | agree |  |
| `string_SUITE:old_to_integer` | 1/0 | 1/0 | agree |  |
| `string_SUITE:old_tokens` | 1/0 | 1/0 | agree |  |
| `string_SUITE:pad` | 1/0 | 1/0 | agree |  |
| `string_SUITE:prefix` | 1/0 | 1/0 | agree |  |
| `string_SUITE:replace` | 1/0 | 1/0 | agree |  |
| `string_SUITE:reverse` | 1/0 | 1/0 | agree |  |
| `string_SUITE:slice` | 1/0 | 1/0 | agree |  |
| `string_SUITE:span_cspan` | 1/0 | 1/0 | agree |  |
| `string_SUITE:split` | 1/0 | 1/0 | agree |  |
| `string_SUITE:str_rstr` | 1/0 | 1/0 | agree |  |
| `string_SUITE:strip` | 1/0 | 1/0 | agree |  |
| `string_SUITE:sub_string` | 1/0 | 1/0 | agree |  |
| `string_SUITE:sub_word` | 1/0 | 1/0 | agree |  |
| `string_SUITE:substr` | 1/0 | 1/0 | agree |  |
| `string_SUITE:take` | 1/0 | 1/0 | agree |  |
| `string_SUITE:titlecase` | 1/0 | 1/0 | agree |  |
| `string_SUITE:to_float` | 1/0 | 1/0 | agree |  |
| `string_SUITE:to_graphemes` | 1/0 | 1/0 | agree |  |
| `string_SUITE:to_integer` | 1/0 | 1/0 | agree |  |
| `string_SUITE:to_upper_to_lower` | 1/0 | 1/0 | agree |  |
| `string_SUITE:trim` | 1/0 | 1/0 | agree |  |
| `string_SUITE:uppercase` | 1/0 | 1/0 | agree |  |
| `string_SUITE:words` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:bin_is_7bit` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:binaries_errors_limit` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{unicode_SUITE,'-ex_binaries_errors_utf8/1-lc$^1/1-1-',6,...}} |
| `unicode_SUITE:category` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:error_info` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:ex_binaries_errors_utf16_big` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{unicode_SUITE,utf16_inner_loop,6,...}} |
| `unicode_SUITE:ex_binaries_errors_utf16_little` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{unicode_SUITE,utf16_inner_loop,6,...}} |
| `unicode_SUITE:ex_binaries_errors_utf32_big` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{unicode_SUITE,utf32_inner_loop,6,...}} |
| `unicode_SUITE:ex_binaries_errors_utf32_little` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{unicode_SUITE,utf32_inner_loop,6,...}} |
| `unicode_SUITE:ex_binaries_errors_utf8` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,false},{unicode_SUITE,'-ex_binaries_errors_utf8/1-lc$^1/1-1-',6,...}} |
| `unicode_SUITE:exceptions` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:huge_illegal_code_points` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:incomplete_utf32_to_latin1` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:is_id` | 0/1 | 0/1 | agree-fail-identical | {exception,error,{badmatch,{error,enoent}},{unicode_SUITE,parse_properties,1,...}} |
| `unicode_SUITE:is_whitespace` | 0/1 | 0/1 | agree-fail-identical | {exception,error,{badmatch,{error,enoent}},{unicode_SUITE,parse_properties,1,...}} |
| `unicode_SUITE:latin1` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:normalize` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:random_lists` | 1/0 | 0/1 | divergence | timeout |
| `unicode_SUITE:roundtrips` | 1/0 | 0/1 | divergence | timeout |
| `unicode_SUITE:utf16_illegal_sequences_bif` | 1/0 | 1/0 | agree |  |
| `unicode_SUITE:utf8_illegal_sequences_bif` | 1/0 | 0/1 | divergence | timeout |

Recorded invocations (the two sides get the *same* `-pa` set and the *same* `-eval`):

```
# ct binary_module_SUITE [sys]
cd /tmp/erts-suites-stage/stdlib/cwd/binary_module_SUITE && /nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl -noshell -pa /tmp/erts-suites-stage/stdlib/ebin -pa /tmp/erts-suites-stage/stdlib/ctl -eval 'ct_runner:run('\''binary_module_SUITE'\'', 10000, 300000, "/tmp/erts-suites-stage/stdlib/src"), halt(0).'
# ct binary_module_SUITE [rust]
cd /tmp/erts-suites-stage/stdlib/cwd/binary_module_SUITE && /home/bill/Source/otp/.genesis/workers/worker_T1_A18/erts_rust/target/release/beam_rt -noshell -pa /tmp/erts-suites-stage/stdlib/ebin -pa /tmp/erts-suites-stage/stdlib/ctl -eval 'ct_runner:run('\''binary_module_SUITE'\'', 10000, 300000, "/tmp/erts-suites-stage/stdlib/src"), halt(0).'
# erlc [build]
/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erlc -o <stage>/ebin -I <stage>/src <suite>.erl …
```

