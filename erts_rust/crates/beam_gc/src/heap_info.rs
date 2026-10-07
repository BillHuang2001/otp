//! Heap observables in the shape of `erlang:process_info(Pid, garbage_collection)`
//! (the `ERTS_PI_IX_GARBAGE_COLLECTION` item, `erts/emulator/beam/erl_bif_info.c`):
//! `minor_gcs`, `fullsweep_after`, `min_heap_size`, `min_bin_vheap_size`, `max_heap_size`,
//! plus the derived `heap_size`/`total_heap_size` the runtime reports.
