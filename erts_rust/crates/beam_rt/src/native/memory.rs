//! `erlang:process_info/2` — the per-process introspection surface (contract A.8).
//!
//! Like the rest of the floor ([`super`]), this is a *runtime* operation: the answer is
//! read out of this runtime's own process table, so it is served here, **before** the
//! `beam_bifs` registry is consulted. The GC observables come from the sibling
//! `beam_gc` crate's [`HeapInfo`] / [`HeapObservable`] seam — `beam_rt` reads the
//! collector, it does not re-derive the item.
//!
//! # The items this runtime answers
//!
//! | item | value |
//! |---|---|
//! | `garbage_collection` | the oracle's five-item proplist (see below) |
//! | `heap_size` | the process's nursery capacity, in words |
//! | `total_heap_size` | the nursery **plus** the mature generation, in words |
//!
//! The reply is always `{Item, Value}`; for a pid that is not in the process table it is
//! the atom `undefined` (what the oracle answers for a dead process).
//!
//! # `garbage_collection`
//!
//! Verified against the oracle (OTP 29 / erts-17.1) for a live, idle spawned process:
//!
//! ```text
//! {garbage_collection,
//!  [{max_heap_size, #{error_logger => true,include_shared_binaries => false,
//!                    kill => true,size => 0}},
//!   {min_bin_vheap_size,46422},
//!   {min_heap_size,233},
//!   {fullsweep_after,65535},
//!   {minor_gcs,0}]}
//! ```
//!
//! The key order is [`beam_gc::GARBAGE_COLLECTION_KEYS`]; `max_heap_size` is the flag
//! **map** here (`size` is the raw word bound, 0 = unlimited), unlike the bare integer
//! `erlang:system_info(garbage_collection)` reports. `min_heap_size` /
//! `min_bin_vheap_size` / `fullsweep_after` come from the **emulator's** heap-size options
//! — the `+hms`/`+hmbs`/`+hmax*`-configured policy the process was seeded with
//! ([`crate::info::effective_policy`]), so `+hms 1000 +hmbs 5000` reports 1598 / 6772 —
//! and `minor_gcs` is the collector's real counter (today always `0`: this runtime runs
//! no GC). Nothing here is hardcoded from the oracle.
//!
//! # Documented divergence: `heap_size` / `total_heap_size`
//!
//! The oracle reports `233` for both on a fresh idle process. This runtime reports its
//! **real** capacities (`Collector::heap_size_words` / `total_heap_size_words`), so an
//! idle process reports `233` for `heap_size` (the young space is
//! `initial_young_words` = 233 words) but `466` for `total_heap_size`, because the
//! collector pre-allocates a 233-word mature generation where the oracle's old heap is
//! still empty. Reporting the real figure — rather than the oracle's — is deliberate: the
//! number is a property of *this* runtime's heap, and inventing `233` would be a lie.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::{HeapInfo, HeapObservable};
use beam_process::ProcessId;
use beam_term::{make_small, Eterm};

use super::RuntimeContext;
use crate::config::MaxHeapFlags;

/// The `erlang:process_info/2` item names this runtime answers.
///
/// Anything else is [`BeamError::BadArg`] — the oracle's `error:badarg` for an unknown
/// item.
const ITEMS: &[&str] = &["garbage_collection", "heap_size", "total_heap_size"];

/// One process's heap/GC observables, copied out of the collector.
///
/// The values are copied rather than borrowed because the answer is built on the
/// engine's heap afterwards, which needs `&mut` access to the runtime the collector lives
/// in.
struct Measurements {
    /// Minor collections run so far (the real counter — `0` until a GC runs).
    minor_gcs: u32,
    /// The nursery's capacity, in words.
    heap_size: usize,
    /// The nursery plus the mature generation, in words.
    total_heap_size: usize,
}

impl RuntimeContext<'_> {
    /// `erlang:process_info(Pid, Item)`.
    ///
    /// The failure modes are ordered as the oracle orders them (verified against OTP 29 /
    /// erts-17.1): an unknown (or non-atom) item is `error:badarg` **even** for a pid that
    /// is not live (`process_info(list_to_pid("<0.999.0>"), nonsense)` ⇒ `badarg`), then a
    /// first argument that is not an internal pid is `error:badarg`
    /// (`process_info(42, garbage_collection)` ⇒ `badarg`), and only a *valid* item with
    /// a *valid* pid that is not in the process table yields the atom `undefined`.
    pub(super) fn process_info_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid_term = *args.first().ok_or(BeamError::BadArg)?;
        let item_term = *args.get(1).ok_or(BeamError::BadArg)?;
        let item = self.atom_name(item_term).ok_or(BeamError::BadArg)?;
        if !ITEMS.contains(&item.as_str()) {
            return Err(BeamError::BadArg);
        }
        // The item is valid, so now the pid must be a pid at all. The oracle's `badarg`
        // for a non-pid wins over any "not found" answer.
        let pid = ProcessId::from_eterm(pid_term).map_err(|_| BeamError::BadArg)?;
        let Some(measurements) = self.measurements(pid) else {
            // A pid that names no live process: the oracle answers the atom `undefined`
            // (e.g. after `exit(P, kill)`), not an error.
            return self.atom("undefined");
        };
        let value = match item.as_str() {
            "garbage_collection" => self.garbage_collection(&measurements)?,
            "heap_size" => make_small(measurements.heap_size as i64),
            "total_heap_size" => make_small(measurements.total_heap_size as i64),
            _ => return Err(BeamError::BadArg),
        };
        // Echo the item atom from the runtime's own table rather than the caller's term,
        // so the reply is canonical whatever atom table the caller matched against.
        let item_atom = self.atom(&item)?;
        terms::make_tuple(self, &[item_atom, value])
    }

    /// `pid`'s heap/GC observables, or `None` when the process table has no such process.
    fn measurements(&self, pid: ProcessId) -> Option<Measurements> {
        let collector = self.runtime.processes().get(pid)?.collector();
        Some(Measurements {
            minor_gcs: collector.minor_gcs(),
            heap_size: collector.heap_size_words(),
            total_heap_size: collector.total_heap_size_words(),
        })
    }

    /// The `garbage_collection` value: the oracle's five `{Key, Value}` pairs, in
    /// [`beam_gc::GARBAGE_COLLECTION_KEYS`] order.
    ///
    /// The bounds come from the **emulator's** heap-size options
    /// ([`effective_policy`](crate::info::effective_policy)), i.e. the `+hms`/`+hmbs`/
    /// `+hmax`-configured policy the process was seeded with — the C's
    /// `p->min_heap_size`/`p->min_bin_vheap_size`/`p->max_heap_size`. The *sizes* are this
    /// process's own measurement.
    fn garbage_collection(&mut self, measurements: &Measurements) -> Result<Eterm> {
        let policy = crate::info::effective_policy(self.runtime);
        let info = HeapInfo::new(
            &policy,
            measurements.minor_gcs,
            measurements.heap_size,
            measurements.total_heap_size,
        );
        let flags = self.runtime.config().heap_options().max_heap_flags();
        let mut entries = Vec::with_capacity(5);
        for (key, value) in info.items() {
            let value = match key {
                // The one item whose value is a container: the flag map.
                "max_heap_size" => self.max_heap_size_map(value, flags)?,
                _ => make_small(value as i64),
            };
            let key = self.atom(key)?;
            entries.push(terms::make_tuple(self, &[key, value])?);
        }
        terms::make_list(self, &entries)
    }

    /// The `max_heap_size` process-flag map: `#{size => N, kill => F, error_logger => F,
    /// include_shared_binaries => F}` — the OTP defaults for a process with no explicit
    /// limit (`kill`/`error_logger` on, shared binaries excluded), overridden by
    /// `+hmaxk`/`+hmaxel`/`+hmaxib`. `size` is the raw word bound (0 = unlimited).
    fn max_heap_size_map(&mut self, size: usize, flags: MaxHeapFlags) -> Result<Eterm> {
        // The oracle's entry order (verified against OTP 29.1.1 / erts-17.1) — the same
        // order the flag *map* is built in, `size` last.
        let booleans = [
            ("error_logger", flags.error_logger),
            ("include_shared_binaries", flags.include_shared_binaries),
            ("kill", flags.kill),
        ];
        let mut pairs = Vec::with_capacity(booleans.len() + 1);
        for (key, flag) in booleans {
            let value = self.atom(bool_atom(flag))?;
            pairs.push((self.atom(key)?, value));
        }
        pairs.push((self.atom("size")?, make_small(size as i64)));
        // An owned handle to the shared table keeps the caller's `&mut self` free for the
        // map builder (`terms::make_map` needs both the context and the table).
        let atoms = self.runtime.code_server.atoms_handle();
        terms::make_map(self, &pairs, &atoms)
    }
}

/// The atom name of a boolean flag value (`true` / `false`).
fn bool_atom(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{atom_val, is_atom, signed_value, NIL};

    use crate::native::test_support::{atom, call, runtime};

    /// The runtime's own root process — a live process with a collector.
    fn root_pid(runtime: &mut crate::Runtime) -> Eterm {
        runtime.self_pid().to_eterm()
    }

    /// The item atom, interned in the runtime's shared table.
    fn item(runtime: &mut crate::Runtime, name: &str) -> Eterm {
        atom(runtime, name)
    }

    /// The `{Key, Value}` pairs of a proplist reply, as `(key name, value term)`.
    fn pairs_of(tuple: Eterm) -> Vec<(Eterm, Eterm)> {
        let arity = terms::tuple_arity(tuple).expect("a reply is a tuple");
        assert_eq!(arity, 2, "a reply is a 2-tuple");
        let proplist = terms::tuple_element(tuple, 1).expect("the value");
        terms::list_elements(proplist)
            .expect("a proplist")
            .iter()
            .map(|entry| {
                (
                    terms::tuple_element(*entry, 0).expect("a key"),
                    terms::tuple_element(*entry, 1).expect("a value"),
                )
            })
            .collect()
    }

    #[test]
    fn garbage_collection_matches_the_oracle_for_a_live_process() {
        let mut runtime = runtime();
        let pid = root_pid(&mut runtime);
        let item = item(&mut runtime, "garbage_collection");
        let reply = call(&mut runtime, "erlang", "process_info", &[pid, item])
            .expect("process_info/2 on the root process");

        let tag = terms::tuple_element(reply, 0).expect("the item");
        assert_eq!(runtime.atom_name_of(tag), Some("garbage_collection"));

        let pairs = pairs_of(reply);
        // The oracle's order and values (OTP 29 / erts-17.1) — `max_heap_size` is the
        // flag map, checked in its own test.
        let expected = [
            ("max_heap_size", None),
            ("min_bin_vheap_size", Some(46_422_i64)),
            ("min_heap_size", Some(233)),
            ("fullsweep_after", Some(65_535)),
            ("minor_gcs", Some(0)),
        ];
        assert_eq!(pairs.len(), expected.len());
        for ((key, value), (name, expected_value)) in pairs.iter().zip(expected) {
            assert_eq!(runtime.atom_name_of(*key), Some(name));
            if let Some(expected_value) = expected_value {
                assert_eq!(signed_value(*value), expected_value, "{name}");
            }
        }
    }

    /// The heap options really reach the per-process item, pinned against the oracle
    /// (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// erl +hms 1000 +hmbs 5000 -noshell \
    ///     -eval 'io:format("~p~n",[process_info(self(),garbage_collection)]), halt().'
    ///   → [{max_heap_size,#{error_logger=>true,include_shared_binaries=>false,kill=>true,
    ///                       size=>0}},
    ///      {min_bin_vheap_size,6772},{min_heap_size,1598},{fullsweep_after,65535},
    ///      {minor_gcs,0}]
    /// erl +hmaxk false +hmaxel false +hmaxib true -noshell -eval … (same probe)
    ///   → the map reads #{error_logger=>false,include_shared_binaries=>true,kill=>false,…}
    /// ```
    #[test]
    fn heap_options_reach_garbage_collection() {
        // The default runtime reports the oracle's 233 / 46422 (see the test above); a
        // `+hms`/`+hmbs` runtime reports the growth-table rounding of the raw request.
        let argv: Vec<String> = ["+hms", "1000", "+hmbs", "5000"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        let config = crate::cli::parse(&argv).expect("+hms 1000 +hmbs 5000 parses");
        let mut runtime = crate::Runtime::new(config).expect("a configured runtime");
        let pid = root_pid(&mut runtime);
        let item_term = item(&mut runtime, "garbage_collection");
        let reply = call(&mut runtime, "erlang", "process_info", &[pid, item_term])
            .expect("process_info/2");
        let pairs = pairs_of(reply);
        let expected = [
            ("max_heap_size", None),
            ("min_bin_vheap_size", Some(6_772_i64)),
            ("min_heap_size", Some(1_598)),
            ("fullsweep_after", Some(65_535)),
            ("minor_gcs", Some(0)),
        ];
        assert_eq!(pairs.len(), expected.len());
        for ((key, value), (name, expected_value)) in pairs.iter().zip(expected) {
            assert_eq!(runtime.atom_name_of(*key), Some(name));
            if let Some(expected_value) = expected_value {
                assert_eq!(signed_value(*value), expected_value, "{name}");
            }
        }

        // … and the `+hmax*` flag word is what the map reports.
        let argv: Vec<String> = ["+hmaxk", "false", "+hmaxel", "false", "+hmaxib", "true"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        let config = crate::cli::parse(&argv).expect("the +hmax* flags parse");
        let mut runtime = crate::Runtime::new(config).expect("a configured runtime");
        let pid = root_pid(&mut runtime);
        let item_term = item(&mut runtime, "garbage_collection");
        let reply = call(&mut runtime, "erlang", "process_info", &[pid, item_term])
            .expect("process_info/2");
        let map = pairs_of(reply)[0].1;
        let atoms = runtime.code_server().atoms();
        let entries = beam_term::map::map_pairs(map, atoms).expect("a map");
        let value_of = |name: &str| -> Eterm {
            entries
                .iter()
                .find(|(key, _)| atoms.name(atom_val(*key)) == Some(name))
                .unwrap_or_else(|| panic!("{name} must be present"))
                .1
        };
        for (name, expected) in [
            ("kill", "false"),
            ("error_logger", "false"),
            ("include_shared_binaries", "true"),
        ] {
            assert_eq!(atoms.name(atom_val(value_of(name))), Some(expected), "{name}");
        }
    }

    #[test]
    fn max_heap_size_is_the_oracle_flag_map() {
        let mut runtime = runtime();
        let pid = root_pid(&mut runtime);
        let item = item(&mut runtime, "garbage_collection");
        let reply =
            call(&mut runtime, "erlang", "process_info", &[pid, item]).expect("process_info/2");
        let pairs = pairs_of(reply);
        assert_eq!(runtime.atom_name_of(pairs[0].0), Some("max_heap_size"));

        // A flat map: `#{error_logger => true, include_shared_binaries => false,
        // kill => true, size => 0}` — the flag *map*, not the bare integer
        // `system_info(garbage_collection)` reports.
        let map = pairs[0].1;
        let atoms = runtime.code_server().atoms();
        let entries = beam_term::map::map_pairs(map, atoms).expect("a map");
        let names: Vec<Option<&str>> = entries
            .iter()
            .map(|(key, _)| atoms.name(atom_val(*key)))
            .collect();
        assert_eq!(
            names,
            vec![
                Some("error_logger"),
                Some("include_shared_binaries"),
                Some("kill"),
                Some("size"),
            ]
        );
        let value_of = |name: &str| -> Eterm {
            entries
                .iter()
                .find(|(key, _)| atoms.name(atom_val(*key)) == Some(name))
                .unwrap_or_else(|| panic!("{name} must be present"))
                .1
        };
        assert!(is_atom(value_of("error_logger")));
        assert_eq!(atoms.name(atom_val(value_of("error_logger"))), Some("true"));
        assert_eq!(
            atoms.name(atom_val(value_of("include_shared_binaries"))),
            Some("false")
        );
        assert_eq!(atoms.name(atom_val(value_of("kill"))), Some("true"));
        assert_eq!(signed_value(value_of("size")), 0);
    }

    #[test]
    fn heap_size_and_total_heap_size_are_the_processes_real_capacities() {
        let mut runtime = runtime();
        let pid = root_pid(&mut runtime);
        // The values the runtime's own collector reports: the answer is a measurement of
        // this process's heap, not the oracle's figure.
        let (heap_size, total_heap_size) = {
            let process = runtime
                .processes()
                .get(runtime.self_pid())
                .expect("the root process is live");
            (
                process.collector().heap_size_words(),
                process.collector().total_heap_size_words(),
            )
        };
        for (name, expected) in [
            ("heap_size", heap_size),
            ("total_heap_size", total_heap_size),
        ] {
            let item = item(&mut runtime, name);
            let reply = call(&mut runtime, "erlang", "process_info", &[pid, item])
                .unwrap_or_else(|error| panic!("process_info(_, {name}): {error}"));
            let tag = terms::tuple_element(reply, 0).expect("the item");
            assert_eq!(runtime.atom_name_of(tag), Some(name));
            assert_eq!(
                signed_value(terms::tuple_element(reply, 1).expect("a value")),
                expected as i64
            );
        }
        // `Process::new` uses `HeapPolicy::default()`: a 233-word nursery, so `heap_size`
        // agrees with the oracle's 233, while the pre-allocated 233-word mature space
        // makes `total_heap_size` 466 — see this module's documented divergence.
        assert_eq!(heap_size, 233);
        assert_eq!(total_heap_size, 466);
    }

    #[test]
    fn a_pid_that_is_not_live_is_undefined() {
        let mut runtime = runtime();
        // A pid the process table never allocated.
        let dead = ProcessId::new(999_999, 0).to_eterm();
        for name in ["garbage_collection", "heap_size", "total_heap_size"] {
            let item = item(&mut runtime, name);
            let reply = call(&mut runtime, "erlang", "process_info", &[dead, item])
                .unwrap_or_else(|error| panic!("process_info(dead, {name}): {error}"));
            assert_eq!(
                runtime.atom_name_of(reply),
                Some("undefined"),
                "{name} of a dead pid is undefined"
            );
        }
    }

    #[test]
    fn an_unknown_item_is_badarg_even_for_a_bogus_pid() {
        let mut runtime = runtime();
        let live = root_pid(&mut runtime);
        let dead = ProcessId::new(999_999, 0).to_eterm();
        let nonsense = item(&mut runtime, "nonsense");
        for (pid, item) in [(live, nonsense), (dead, nonsense)] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "process_info", &[pid, item]),
                    Err(BeamError::BadArg)
                ),
                "an unknown item must be badarg"
            );
        }
        // A non-atom item is `badarg` too, for a live and a dead pid alike.
        for pid in [live, dead] {
            assert!(matches!(
                call(&mut runtime, "erlang", "process_info", &[pid, make_small(7)]),
                Err(BeamError::BadArg)
            ));
        }
    }

    #[test]
    fn a_non_pid_first_argument_is_badarg() {
        let mut runtime = runtime();
        let item = item(&mut runtime, "garbage_collection");
        for not_a_pid in [make_small(42), atom(&mut runtime, "erlang"), NIL] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "process_info", &[not_a_pid, item]),
                    Err(BeamError::BadArg)
                ),
                "a non-pid process must be badarg"
            );
        }
    }
}