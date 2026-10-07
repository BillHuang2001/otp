//! `erlang:system_info/1` against the **real** oracle (contract A.8).
//!
//! The compatibility gate for this workstream: the value *shape* of every key this
//! runtime answers must be the shape the system `erl` returns, and for the keys that
//! are genuine invariants of the ERTS interface (or of the host this test and the
//! oracle share) the **value** must match too.
//!
//! What is compared, and why:
//!
//! * [`ORACLE_EQUAL_KEYS`] — the oracle's printed value must equal this runtime's
//!   rendered value (whitespace-normalized, because `~p` wraps long lists across
//!   lines). These are either interface constants any erts-17.1 build reports the same
//!   way (`nif_version`, `driver_version`, the limits, the GC policy rows, …) or facts
//!   about the shared host (`wordsize`, `os_type`, `os_version`, `machine`).
//! * [`SHAPE_ONLY_KEYS`] — only the *shape* is compared, never the value. These split
//!   into two honest groups:
//!     1. this runtime's own version strings, which are deliberately its **own**
//!        (`version` = our erts version, `otp_release` = our OTP major, `emu_type` =
//!        our build tier, `debug_compiled` = our debug tier);
//!     2. keys that read **this runtime's own live tables** (`atom_count`,
//!        `process_count`, `port_count`, `ets_count`, `allocated_areas`, …) and are
//!        therefore correct-but-smaller than the oracle's run-time figure. Comparing
//!        them for equality would be comparing two different emulators' tables — the
//!        same rule `erlang:processes/0` / `erlang:ports/0` already follow.
//! * [`UNSUPPORTED_KEYS`] — the oracle **accepts** each of these; this runtime answers
//!   an honest capability gap instead, so the test pins the oracle-accepts fact and
//!   (in `src/info.rs`) the exit-3 classification. The divergence is deliberate.
//! * [`ORACLE_REJECTED_KEYS`] — the oracle raises `error:badarg`, and so must we.
//!
//! The test skips **loudly** (prints to stderr and returns) when no system `erl` is
//! executable — the oracle is an external toolchain, not a build prerequisite.

use std::collections::HashMap;
use std::process::Command;

use beam_rt::info::{system_info_value, InfoValue, SUPPORTED_KEYS, UNSUPPORTED_KEYS};
use beam_rt::{Runtime, RuntimeConfig};

/// Keys whose **value** must equal the oracle's. Every entry is an interface constant
/// of an erts-17.1 build or a fact about the host the test and the oracle share.
const ORACLE_EQUAL_KEYS: &[&str] = &[
    "driver_version",
    "nif_version",
    "wordsize",
    "machine",
    "threads",
    "smp_support",
    "async_dist",
    "os_type",
    "os_version",
    "break_ignored",
    "dynamic_trace",
    "dynamic_trace_probes",
    "modified_timing_level",
    "update_cpu_info",
    "trace_control_word",
    "port_parallelism",
    "kernel_poll",
    "halt_flush_timeout",
    "creation",
    "emu_flavor",
    "atom_limit",
    "process_limit",
    "port_limit",
    "ets_limit",
    "outstanding_system_requests_limit",
    "dist_buf_busy_limit",
    "delayed_node_table_gc",
    "heap_type",
    "message_queue_data",
    "scheduler_bind_type",
    "thread_pool_size",
    "min_heap_size",
    "min_bin_vheap_size",
    "fullsweep_after",
    "garbage_collection",
    "alloc_util_allocators",
    "multi_scheduling_blockers",
    "normal_multi_scheduling_blockers",
];

/// Keys for which only the **shape** is comparable: this runtime's own version strings
/// and the keys derived from its own live tables (see the module docs).
const SHAPE_ONLY_KEYS: &[&str] = &[
    "version",
    "otp_release",
    "system_architecture",
    "system_version",
    "emu_type",
    "debug_compiled",
    "atom_count",
    "process_count",
    "port_count",
    "ets_count",
    "logical_processors",
    "logical_processors_available",
    "logical_processors_online",
    "schedulers",
    "schedulers_online",
    "scheduler_id",
    "dirty_cpu_schedulers",
    "dirty_cpu_schedulers_online",
    "dirty_io_schedulers",
    "scheduler_bindings",
    "multi_scheduling",
    "max_heap_size",
    "heap_sizes",
    "allocated_areas",
];

/// Keys the oracle accepts that this runtime answers with an honest capability gap.
/// Pinned here so a future `unsupported` key is checked against the oracle too.
const ORACLE_ACCEPTED_UNSUPPORTED_KEYS: &[&str] = &[
    "allocator",
    "dist",
    "dist_ctrl",
    "time_offset",
    "time_correction",
    "time_warp_mode",
    "tolerant_timeofday",
    "start_time",
    "end_time",
    "os_monotonic_time_source",
    "os_system_time_source",
    "cpu_topology",
    "check_io",
    "c_compiler_used",
    "embedded_3pps",
    "system_logger",
    "info",
    "procs",
    "loaded",
];

/// Keys the oracle raises `error:badarg` for — this runtime must not invent an answer.
const ORACLE_REJECTED_KEYS: &[&str] = &[
    "allocator_sizes",
    "beam_jit_available",
    "busy_limits",
    "c_library_version",
    "catchlevel",
    "export_count",
    "export_limit",
    "memory",
    "module_count",
    "module_limit",
    "net_ticktime",
    "node",
    "pure",
    "run_queue",
    "safe_string_hash",
    "total_run_queue_lengths",
    "wall_clock",
];

// -- shape ---------------------------------------------------------------------

/// The coarse Erlang type of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Str,
    Atom,
    Tuple,
    List,
    Map,
}

/// A value's observable shape: its kind plus (for containers) its top-level arity.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shape {
    kind: Kind,
    arity: usize,
}

/// The shape of an [`InfoValue`]. `Bool` materializes as the atom `true`/`false`, so it
/// shares `Kind::Atom` with `InfoValue::Atom` — exactly as the oracle prints it.
fn rust_shape(value: &InfoValue) -> Shape {
    match value {
        InfoValue::Int(_) => Shape {
            kind: Kind::Int,
            arity: 0,
        },
        InfoValue::Str(_) => Shape {
            kind: Kind::Str,
            arity: 0,
        },
        InfoValue::Atom(_) | InfoValue::Bool(_) => Shape {
            kind: Kind::Atom,
            arity: 0,
        },
        InfoValue::Tuple(_, _) => Shape {
            kind: Kind::Tuple,
            arity: 2,
        },
        InfoValue::Tupled(elements) => Shape {
            kind: Kind::Tuple,
            arity: elements.len(),
        },
        InfoValue::List(elements) => Shape {
            kind: Kind::List,
            arity: elements.len(),
        },
        InfoValue::Map(pairs) => Shape {
            kind: Kind::Map,
            arity: pairs.len(),
        },
    }
}

/// The number of top-level elements of an already-delimited container body.
fn arity(inner: &str) -> usize {
    let inner = inner.trim();
    if inner.is_empty() {
        return 0;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut count = 1usize;
    for c in inner.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

/// The shape of an oracle `~p` rendering. The values the corpus probes are all
/// literals/int/atom/string/tuple/list/map, so the classifier only has to look at the
/// outermost delimiters.
fn oracle_shape(text: &str) -> Shape {
    let text = text.trim();
    let scalar = |kind| Shape { kind, arity: 0 };
    if text.starts_with('"') {
        return scalar(Kind::Str);
    }
    if let Some(inner) = delimited(text, "#{", "}") {
        return Shape {
            kind: Kind::Map,
            arity: arity(inner),
        };
    }
    if let Some(inner) = delimited(text, "{", "}") {
        return Shape {
            kind: Kind::Tuple,
            arity: arity(inner),
        };
    }
    if let Some(inner) = delimited(text, "[", "]") {
        return Shape {
            kind: Kind::List,
            arity: arity(inner),
        };
    }
    if text.parse::<i64>().is_ok() {
        return scalar(Kind::Int);
    }
    scalar(Kind::Atom)
}

/// The body of `text` when it is delimited by `open` … `close`.
fn delimited<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    text.strip_prefix(open)?.strip_suffix(close)
}

// -- rendering -----------------------------------------------------------------

/// Renders an [`InfoValue`] the way `~p` renders the equivalent term (`InfoValue::Map`
/// has no comparable rendering, and no key of this crate is compared as text by map).
fn render(value: &InfoValue) -> String {
    match value {
        InfoValue::Int(number) => number.to_string(),
        InfoValue::Str(text) => format!("\"{text}\""),
        InfoValue::Atom(name) => (*name).to_string(),
        InfoValue::Bool(value) => if *value { "true" } else { "false" }.to_string(),
        InfoValue::Tuple(first, second) => format!("{{{first},{second}}}"),
        InfoValue::Tupled(elements) => {
            let parts: Vec<String> = elements.iter().map(render).collect();
            format!("{{{}}}", parts.join(","))
        }
        InfoValue::List(elements) => {
            let parts: Vec<String> = elements.iter().map(render).collect();
            format!("[{}]", parts.join(","))
        }
        // No map-valued key is text-compared (see `Shape::Map`): `~p` orders map keys
        // by term order and the pair order is not part of this test's contract.
        InfoValue::Map(pairs) => {
            let parts: Vec<String> = pairs
                .iter()
                .map(|(key, value)| format!("{} => {}", render(key), render(value)))
                .collect();
            format!("#{{{}}}", parts.join(","))
        }
    }
}

/// A whitespace-free comparison form. `~p` wraps long lists across lines with
/// alignment spaces, which is presentation only — and every key compared this way
/// contains no string element, so dropping whitespace cannot hide a real difference.
fn normalize(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

// -- the oracle ----------------------------------------------------------------

/// The oracle's answers for `keys`, in one `erl` invocation.
///
/// Each key is printed as `<key>\t<~p of catch system_info(Key)>`; the records of a
/// value that `~p` wrapped across lines are stitched back together. Returns `None` when
/// no `erl` can be executed (the caller skips loudly).
fn oracle_answers(keys: &[&str]) -> Option<HashMap<String, String>> {
    let list = keys
        .iter()
        .map(|key| format!("'{key}'"))
        .collect::<Vec<_>>()
        .join(",");
    let expr = format!(
        "K = [{list}], \
         lists:foreach(fun(K0) -> \
             io:format(\"~s\\t~p~n\", [K0, catch erlang:system_info(K0)]) \
         end, K), halt(0)."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &expr])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();

    let mut answers = HashMap::new();
    let mut current: Option<(String, Vec<String>)> = None;
    for line in text.lines() {
        let starts_a_record = line
            .split_once('\t')
            .filter(|(key, _)| {
                !key.is_empty()
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && keys.contains(key)
            })
            .map(|(key, rest)| (key.to_string(), rest.to_string()));
        match starts_a_record {
            Some((key, rest)) => {
                if let Some((previous, lines)) = current.take() {
                    answers.insert(previous, lines.join(" "));
                }
                current = Some((key, vec![rest]));
            }
            None => {
                if let Some((_, lines)) = current.as_mut() {
                    lines.push(line.to_string());
                }
            }
        }
    }
    if let Some((key, lines)) = current {
        answers.insert(key, lines.join(" "));
    }
    Some(answers)
}

/// Whether the oracle refused a key (`catch` wrapped its `error:badarg` in `{'EXIT',…}`).
fn oracle_rejected(text: &str) -> bool {
    text.trim_start().starts_with("{'EXIT'")
}

#[test]
fn the_key_sets_partition_the_supported_surface() {
    // A key added to `SUPPORTED_KEYS` but not classified here would silently escape the
    // oracle comparison — so the partition itself is the first assertion.
    let mut classified: Vec<&str> = ORACLE_EQUAL_KEYS
        .iter()
        .chain(SHAPE_ONLY_KEYS)
        .copied()
        .collect();
    classified.sort_unstable();
    let mut supported: Vec<&str> = SUPPORTED_KEYS.to_vec();
    supported.sort_unstable();
    assert_eq!(classified, supported);

    let mut unsupported: Vec<&str> = UNSUPPORTED_KEYS.to_vec();
    unsupported.sort_unstable();
    let mut accepted: Vec<&str> = ORACLE_ACCEPTED_UNSUPPORTED_KEYS.to_vec();
    accepted.sort_unstable();
    assert_eq!(accepted, unsupported);
}

#[test]
fn the_oracle_shapes_and_values_match() {
    let keys: Vec<&str> = ORACLE_EQUAL_KEYS
        .iter()
        .chain(SHAPE_ONLY_KEYS)
        .chain(ORACLE_ACCEPTED_UNSUPPORTED_KEYS.iter())
        .chain(ORACLE_REJECTED_KEYS.iter())
        .copied()
        .collect();
    let Some(answers) = oracle_answers(&keys) else {
        eprintln!("SKIP the_oracle_shapes_and_values_match: no system `erl` to compare with");
        return;
    };

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    rt.boot().expect("boot");

    // -- the oracle's own verdicts, so the comparison below is meaningful ----------
    for key in ORACLE_ACCEPTED_UNSUPPORTED_KEYS {
        let oracle = answers
            .get(*key)
            .unwrap_or_else(|| panic!("the oracle did not answer {key}"));
        assert!(
            !oracle_rejected(oracle),
            "the oracle must accept {key} (we answer a capability gap instead): {oracle}"
        );
    }
    for key in ORACLE_REJECTED_KEYS {
        let oracle = answers
            .get(*key)
            .unwrap_or_else(|| panic!("the oracle did not answer {key}"));
        assert!(
            oracle_rejected(oracle),
            "the oracle must reject {key} (we must stay badarg): {oracle}"
        );
    }

    // -- keys whose value must match exactly ---------------------------------------
    for key in ORACLE_EQUAL_KEYS {
        let oracle = answers
            .get(*key)
            .unwrap_or_else(|| panic!("the oracle did not answer {key}"));
        let value = system_info_value(key, &rt)
            .unwrap_or_else(|error| panic!("system_info({key}) failed: {error}"));
        assert_eq!(
            rust_shape(&value),
            oracle_shape(oracle),
            "system_info({key}): shape mismatch (ours {value:?}), oracle said {oracle}"
        );
        assert_eq!(
            normalize(&render(&value)),
            normalize(oracle),
            "system_info({key}): value mismatch, oracle said {oracle}"
        );
    }

    // -- keys whose shape must match (the value is this runtime's own) -------------
    for key in SHAPE_ONLY_KEYS {
        let oracle = answers
            .get(*key)
            .unwrap_or_else(|| panic!("the oracle did not answer {key}"));
        let value = system_info_value(key, &rt)
            .unwrap_or_else(|error| panic!("system_info({key}) failed: {error}"));
        let ours = rust_shape(&value);
        let theirs = oracle_shape(oracle);
        assert_eq!(
            ours.kind, theirs.kind,
            "system_info({key}): kind mismatch (ours {value:?}), oracle said {oracle}"
        );
        // Scalars have no arity to compare; containers do, except the two whose arity
        // legitimately follows *this* runtime's scheduler count / growth table.
        if theirs.arity != 0 && !matches!(*key, "scheduler_bindings" | "heap_sizes") {
            assert_eq!(
                ours.arity, theirs.arity,
                "system_info({key}): arity mismatch (ours {value:?}), oracle said {oracle}"
            );
        }
    }

    // -- this runtime's own version strings, pinned against its own constants ------
    assert_eq!(
        system_info_value("version", &rt).expect("version"),
        InfoValue::Str(beam_common::ERTS_VERSION.to_string())
    );
    assert_eq!(
        system_info_value("otp_release", &rt).expect("otp_release"),
        InfoValue::Str(beam_common::otp_version().major.to_string())
    );
}

#[test]
fn self_derived_counts_stay_shaped_like_the_oracle_even_thinned() {
    // The counts come from this runtime's own tables, so they are smaller than the
    // oracle's — but they must still be plain non-negative integers, and the arity of a
    // self-derived container must equal this runtime's own scheduler count.
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    rt.boot().expect("boot");

    for key in ["atom_count", "process_count", "port_count", "ets_count"] {
        let value = system_info_value(key, &rt).unwrap_or_else(|e| panic!("{key}: {e}"));
        assert!(
            matches!(value, InfoValue::Int(count) if count >= 0),
            "{key} must be a non-negative integer, got {value:?}"
        );
    }
    let schedulers = match system_info_value("schedulers", &rt).expect("schedulers") {
        InfoValue::Int(count) => count,
        other => panic!("schedulers must be an integer, got {other:?}"),
    };
    match system_info_value("scheduler_bindings", &rt).expect("scheduler_bindings") {
        InfoValue::Tupled(elements) => {
            assert_eq!(elements.len() as i64, schedulers);
            assert!(elements.iter().all(|e| *e == InfoValue::Atom("unbound")));
        }
        other => panic!("scheduler_bindings must be a tuple, got {other:?}"),
    }
}

#[test]
fn the_oracle_refusal_form_is_recognised() {
    // `catch erlang:system_info(BadKey)` yields `{'EXIT',{badarg,…}}`, which has the same
    // *shape* as a legitimate 2-tuple. The oracle-accepts / oracle-rejects verdicts above
    // therefore key off the printed form, not the shape — pinned here so the distinction
    // cannot silently regress.
    assert!(oracle_rejected("{'EXIT',{badarg,[]}}"));
    assert!(oracle_rejected("  {'EXIT',{undef,[]}}"));
    assert!(!oracle_rejected("{unix,linux}"));
    assert!(!oracle_rejected("[]"));
    assert_eq!(
        oracle_shape("{'EXIT',{badarg,[]}}").kind,
        oracle_shape("{unix,linux}").kind
    );
}