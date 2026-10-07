//! `erlang:process_info/1,2` — the per-process introspection surface (contract A.8).
//!
//! Like the rest of the floor ([`super`]), this is a *runtime* operation: the answer is
//! read out of this runtime's own process table, so it is served here, **before** the
//! `beam_bifs` registry is consulted.
//!
//! # The two shapes the oracle answers
//!
//! `erlang:process_info/2` accepts an **item atom**, a **list of items** or the 2-tuple
//! `{dictionary, Key}`:
//!
//! * `process_info(Pid, Item)` → `{Item, Value}` — except `registered_name`, whose value is
//!   the **bare** name atom, and the bare `[]` when the process is **unregistered**. The C's
//!   `always_wrap = !is_atom(opt)` (`erl_bif_info.c:1556`) is what makes that one item
//!   unwrapped in the single-atom form.
//! * `process_info(Pid, [Item, …])` → `[{Item, Value}, …]`, in the requested order, with
//!   duplicates repeated. Here `registered_name` **is** wrapped in both cases:
//!   `{registered_name, Name}` or `{registered_name, []}`.
//! * `process_info(Pid, {dictionary, Key})` → `{{dictionary, Key}, Value|undefined}`.
//!
//! `erlang:process_info/1` answers the **fixed** key set of the C's `pi_1_keys[]`
//! (`erl_bif_info.c:1008-1026`), in that exact order. For a **registered** process it emits
//! all 17 items, `{registered_name, Name}` first; for an **unregistered** one it **omits**
//! `registered_name` and emits the remaining 16. (The C skips it because `always_wrap = 0`
//! makes an unregistered `registered_name` the bare `[]`, which the multi-item loop treats as
//! "no value": `erl_bif_info.c:1175`.) This runtime further omits the two engine-derived items
//! it honestly cannot answer (`current_function`/`initial_call`), so its count is 2 fewer than
//! the oracle's in each case (15 registered, 14 unregistered).
//!
//! # The failure modes (verified against OTP 29.1.1 / erts-17.1)
//!
//! * a non-atom, non-list, non-`{dictionary,_}` second argument ⇒ `error:badarg`;
//! * an **unknown item name** (or an unknown item *inside* a list, or an improper list) ⇒
//!   `error:badarg`, **even** for a pid that is not live;
//! * a first argument that is not an internal pid ⇒ `error:badarg`;
//! * a valid item with a valid pid that is **not in the process table** ⇒ the atom
//!   `undefined` (for the single form, the list form and `process_info/1` alike — the list
//!   form of a dead pid is `undefined`, *not* `[]`).
//!
//! # Items this runtime answers
//!
//! | item | shape |
//! |---|---|
//! | `registered_name` | `{registered_name, Name}` when registered; the bare `[]` (unregistered) in the `/2`-atom form, `{registered_name, []}` in the list form, omitted by `/1` |
//! | `status` | `running`/`runnable`/`waiting`/`suspended`/`exiting` from [`ProcessState`] |
//! | `message_queue_len` | the mailbox length |
//! | `messages` | the queued payload terms, FIFO |
//! | `links` | the linked pids |
//! | `monitors` | `[{process\|port, Pid}]` |
//! | `monitored_by` | the watching pids |
//! | `dictionary` | the `{Key, Value}` pairs, insertion order |
//! | `{dictionary, Key}` | the stored value, else `undefined` |
//! | `trap_exit` | the `trap_exit` flag |
//! | `error_handler` | the atom `error_handler` (an unchanging runtime default) |
//! | `priority` | the scheduling priority atom |
//! | `group_leader` | this runtime's root pid (what `erlang:group_leader/0` answers) |
//! | `total_heap_size` / `heap_size` | this process's real heap capacities, in words |
//! | `stack_size` | the Erlang stack's word count |
//! | `reductions` | this process's real reduction counter |
//! | `garbage_collection` | the oracle's five-item proplist (see below) |
//! | `min_heap_size` / `min_bin_vheap_size` / `fullsweep_after` | the emulator's heap policy |
//! | `max_heap_size` | the process flag **map** |
//! | `message_queue_data` | `on_heap` (an unchanging runtime default — see below) |
//! | `trace` | `0` (no tracing subsystem in M1 — an unchanging default) |
//! | `suspending` | `[]` (no `suspend_process` in M1) |
//! | `last_calls` | `false` (no tracing subsystem in M1) |
//! | `memory` | this process's real bytes (heap + stack + off-heap) |
//!
//! ## Items this runtime honestly cannot answer (`unsupported`)
//!
//! These are *valid* oracle item names, so answering `badarg` would be a lie. Each is a
//! typed capability gap ([`crate::error::unsupported`], reason atom starting with
//! `unsupported`) naming the accessor/hook that is missing:
//!
//! | item | why / what is needed |
//! |---|---|
//! | `current_function`, `current_location`, `current_stacktrace` | the executing code's function/pc: the engine `Machine` is moved **out** of the `Runtime` for the duration of a scheduler pass, so `RuntimeContext` cannot see it; the engine exposes no current-function/pc accessor either |
//! | `initial_call` | no `$initial_call`/initial-MFA field exists on `beam_process::Process` (the M1 PCB models neither) |
//! | `catchlevel` | the runtime has no catch-frame store reachable from here |
//! | `sequential_trace_token`, `trace`-token family | no `seq_trace` subsystem |
//! | `backtrace` | no binary backtrace (see `current_stacktrace`) |
//! | `magic_ref`, `parent`, `label` | no such PCB fields in M1 |
//! | `async_dist`, `priority_messages` | no such process flags in M1 |
//! | `garbage_collection_info` | needs the collector's per-generation block detail beyond [`beam_gc::HeapInfo`] |
//! | `binary` | no process registers off-heap refc binaries in its off-heap table yet (see the crate CONTEXT.md), and `beam_alloc::RefcBinary` exposes no refcount accessor for the `{Ptr,Size,Refc}` third field |
//!
//! ## `garbage_collection`
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
//! `erlang:system_info(garbage_collection)` reports. `min_heap_size` / `min_bin_vheap_size` /
//! `fullsweep_after` come from the **emulator's** heap-size options — the
//! `+hms`/`+hmbs`/`+hmax*`-configured policy the process was seeded with
//! ([`crate::info::effective_policy`]), so `+hms 1000 +hmbs 5000` reports 1598 / 6772 — and
//! `minor_gcs` is the collector's real counter (today always `0`: this runtime runs no GC).
//! Nothing here is hardcoded from the oracle.
//!
//! # Documented divergences (correct-but-different-by-construction)
//!
//! * `heap_size` / `total_heap_size`: the oracle reports `233` for both on a fresh idle
//!   process. This runtime reports its **real** capacities, so an idle process reports `233`
//!   for `heap_size` (the young space is `initial_young_words` = 233 words) but `466` for
//!   `total_heap_size`, because the collector pre-allocates a 233-word mature generation
//!   where the oracle's old heap is still empty. Reporting the real figure is deliberate:
//!   the number is a property of *this* runtime's heap, and inventing `233` would be a lie.
//! * `memory`: this runtime's own byte total (`(total_heap_size + stack_size) * 8` plus the
//!   off-heap overhead), which is smaller than the oracle's because this heap is smaller.
//! * `status` reflects *this* scheduler's [`ProcessState`]; a process our runner has parked
//!   in `receive` is `waiting`, exactly the oracle's answer for a blocked process.
//! * `group_leader` is this runtime's root pid — the same real, live pid
//!   `erlang:group_leader/0` answers — not the oracle's `<0.70.0>` io process.
//! * `reductions` is *this* process's real counter, whose absolute value differs from the
//!   oracle's because the executed code differs.
//! * `message_queue_data` is reported as `on_heap`: this runtime's [`beam_message::Queue`] is
//!   a heap-resident FIFO, so `on_heap` is the true current placement (there is no
//!   `message_queue_data` process flag in M1 that could change it).

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_gc::{HeapInfo, HeapObservable};
use beam_process::{MonitorKind, ProcessId, ProcessState};
use beam_term::{make_small, Eterm, NIL};

use super::RuntimeContext;
use crate::config::MaxHeapFlags;

/// The C's `pi_1_keys[]` (`erl_bif_info.c:1008-1026`), in order — the item set *and* order
/// `erlang:process_info/1` answers. `registered_name` is skipped when the process is
/// unregistered (the C's `nil` skip), which is why the oracle emits 16 items.
const PI_1_KEYS: &[&str] = &[
    "registered_name",
    "current_function",
    "initial_call",
    "status",
    "message_queue_len",
    "links",
    "dictionary",
    "trap_exit",
    "error_handler",
    "priority",
    "group_leader",
    "total_heap_size",
    "heap_size",
    "stack_size",
    "reductions",
    "garbage_collection",
    "suspending",
];

/// The item names this runtime answers with a genuine value from its own state.
const IMPLEMENTED_ITEMS: &[&str] = &[
    "registered_name",
    "status",
    "message_queue_len",
    "messages",
    "links",
    "monitors",
    "monitored_by",
    "dictionary",
    "trap_exit",
    "error_handler",
    "priority",
    "group_leader",
    "total_heap_size",
    "heap_size",
    "stack_size",
    "reductions",
    "garbage_collection",
    "min_heap_size",
    "min_bin_vheap_size",
    "max_heap_size",
    "fullsweep_after",
    "message_queue_data",
    "trace",
    "suspending",
    "last_calls",
    "memory",
];

/// The valid oracle item names this runtime cannot yet answer — a typed `unsupported`
/// capability gap rather than a fabricated value or a false `badarg` (see the module docs).
const UNSUPPORTED_ITEMS: &[&str] = &[
    "current_function",
    "current_location",
    "current_stacktrace",
    "initial_call",
    "catchlevel",
    "sequential_trace_token",
    "backtrace",
    "magic_ref",
    "parent",
    "label",
    "async_dist",
    "priority_messages",
    "garbage_collection_info",
    "binary",
];

/// One parsed `process_info` item: an atom name, or the `{dictionary, Key}` 2-tuple.
enum Item {
    /// An item atom.
    Atom(String),
    /// `{dictionary, Key}`.
    DictionaryKey(Eterm),
}

impl Item {
    /// The term the reply's `{Item, Value}` pair uses as its first element.
    fn key(&self, ctx: &mut RuntimeContext<'_>) -> Result<Eterm> {
        match self {
            Item::Atom(name) => ctx.atom(name),
            Item::DictionaryKey(key) => {
                let tag = ctx.atom("dictionary")?;
                terms::make_tuple(ctx, &[tag, *key])
            }
        }
    }
}

/// One process's observables, copied out of the process table.
///
/// Copied rather than borrowed because the reply is built on the engine's heap afterwards,
/// which needs `&mut` access to the runtime the process table lives in.
struct Snapshot {
    /// The registered name, `None` when unregistered.
    registered_name: Option<String>,
    /// The scheduler state, as its oracle atom.
    status: &'static str,
    /// The mailbox length.
    message_queue_len: usize,
    /// The queued payloads, FIFO.
    messages: Vec<Eterm>,
    /// The linked pids.
    links: Vec<Eterm>,
    /// The outgoing monitors as `(is_process, target_pid)`.
    monitors: Vec<(bool, Eterm)>,
    /// The watching pids.
    monitored_by: Vec<Eterm>,
    /// The process dictionary pairs, insertion order.
    dictionary: Vec<(Eterm, Eterm)>,
    /// The `trap_exit` flag.
    trap_exit: bool,
    /// The scheduling priority atom.
    priority: &'static str,
    /// The group leader (this runtime's root pid).
    group_leader: Eterm,
    /// The nursery capacity, in words.
    heap_size: usize,
    /// The nursery plus the mature generation, in words.
    total_heap_size: usize,
    /// The Erlang stack's word count.
    stack_size: usize,
    /// The real reduction counter.
    reductions: u64,
    /// Minor collections run so far.
    minor_gcs: u32,
    /// The off-heap (MSO + refc binary) overhead, in bytes.
    off_heap_bytes: usize,
}

/// The oracle atom for a [`ProcessState`] — the C's `erts_process_state2status`.
fn status_atom(state: ProcessState) -> &'static str {
    match state {
        ProcessState::Running => "running",
        ProcessState::Runnable => "runnable",
        ProcessState::Waiting => "waiting",
        ProcessState::Suspended => "suspended",
        ProcessState::Exiting | ProcessState::Terminated => "exiting",
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

impl RuntimeContext<'_> {
    /// `erlang:process_info(Pid)`.
    ///
    /// Answers the fixed [`PI_1_KEYS`] set in that order (all 17 keys for a **registered**
    /// process — `{registered_name, Name}` first — and 16, with `registered_name` omitted, for
    /// an **unregistered** one), `undefined` for a pid that is not in the process table and
    /// `badarg` for a non-pid.
    /// An item this runtime cannot answer is **omitted** — the same mechanism the C uses to
    /// drop an unregistered `registered_name` (see the module docs) — so the list is a
    /// genuine, shorter-by-construction answer rather than a fabricated one; the omitted
    /// names are exactly [`UNSUPPORTED_ITEMS`].
    pub(super) fn process_info_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid_term = *args.first().ok_or(BeamError::BadArg)?;
        let pid = ProcessId::from_eterm(pid_term).map_err(|_| BeamError::BadArg)?;
        let Some(snapshot) = self.snapshot(pid) else {
            return self.atom("undefined");
        };
        let mut entries = Vec::with_capacity(PI_1_KEYS.len());
        for name in PI_1_KEYS {
            if *name == "registered_name" {
                match &snapshot.registered_name {
                    // The C's `nil` skip (`erl_bif_info.c:1175`): an unregistered process omits the item.
                    None => continue,
                    Some(_) => {
                        let value = self.registered_name_value(&snapshot)?;
                        let key = self.atom(name)?;
                        entries.push(terms::make_tuple(self, &[key, value])?);
                        continue;
                    }
                }
            }
            let item = Item::Atom((*name).to_string());
            match self.raw_value(&item, &snapshot)? {
                Some(value) => {
                    let key = self.atom(name)?;
                    entries.push(terms::make_tuple(self, &[key, value])?);
                }
                // An item this runtime cannot answer is omitted (honest omission).
                None => continue,
            }
        }
        terms::make_list(self, &entries)
    }

    /// `erlang:process_info(Pid, ItemOrItems)`.
    ///
    /// `ItemOrItems` may be an item **atom**, a **list** of items, or the 2-tuple
    /// `{dictionary, Key}` — exactly the three forms the C accepts
    /// (`erl_bif_info.c:1294-1354`).
    pub(super) fn process_info_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid_term = *args.first().ok_or(BeamError::BadArg)?;
        let option = *args.get(1).ok_or(BeamError::BadArg)?;

        // Validate and parse the option **before** touching the pid: the C's `badarg` for an
        // unknown item wins over the `undefined` answer for a dead pid.
        let parsed = self.parse_option(option)?;

        let pid = ProcessId::from_eterm(pid_term).map_err(|_| BeamError::BadArg)?;
        let Some(snapshot) = self.snapshot(pid) else {
            return self.atom("undefined");
        };

        match parsed {
            ParsedOption::Single(item) => {
                // The C's `always_wrap = !is_atom(opt)` is clear in the single-atom form, so an
                // *unregistered* `registered_name` is the bare `[]` — not `{registered_name, []}`
                // (verified against OTP 29.1.1: `process_info(P, registered_name)` ⇒ `[]`).
                if matches!(&item, Item::Atom(name) if name == "registered_name")
                    && snapshot.registered_name.is_none()
                {
                    return Ok(NIL);
                }
                let value = self.item_value(&item, &snapshot, false)?;
                let key = item.key(self)?;
                terms::make_tuple(self, &[key, value])
            }
            ParsedOption::List(items) => {
                let mut entries = Vec::with_capacity(items.len());
                for item in &items {
                    let value = self.item_value(item, &snapshot, true)?;
                    let key = item.key(self)?;
                    entries.push(terms::make_tuple(self, &[key, value])?);
                }
                terms::make_list(self, &entries)
            }
        }
    }

    /// The value of one item, honoring the `always_wrap` split for `registered_name`.
    ///
    /// `wrapped` is the C's `always_wrap` flag: when it is set (the list/tuple forms),
    /// `registered_name` answers the name atom or `[]`; when it is clear (the single-atom
    /// form), it answers the name atom or the bare `[]`.
    fn item_value(
        &mut self,
        item: &Item,
        snapshot: &Snapshot,
        wrapped: bool,
    ) -> Result<Eterm> {
        if let Item::Atom(name) = item {
            if name == "registered_name" {
                return self.registered_name_value(snapshot);
            }
        }
        let _ = wrapped;
        match self.raw_value(item, snapshot)? {
            Some(value) => Ok(value),
            None => Err(self.unsupported_item(item)),
        }
    }

    /// The `registered_name` value: the **bare** name atom, or the empty list `[]`.
    ///
    /// The C returns this unwrapped whenever `always_wrap` is clear and the item is not
    /// skipped; in the list form it is wrapped by the caller, so the bare value is what both
    /// paths want here.
    fn registered_name_value(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        match &snapshot.registered_name {
            Some(name) => {
                let name = name.clone();
                self.atom(&name)
            }
            None => Ok(NIL),
        }
    }

    /// The typed capability gap for a valid item this runtime cannot answer.
    fn unsupported_item(&self, item: &Item) -> BeamError {
        match item {
            Item::Atom(name) => crate::error::unsupported(&format!(
                "process_info: {name} (see beam_rt/src/native/memory/mod.rs for the missing accessor)"
            )),
            Item::DictionaryKey(_) => crate::error::unsupported("process_info: {dictionary,_}"),
        }
    }

    /// An item's value from this runtime's real state, or `None` when the item is a valid
    /// oracle name this runtime cannot answer.
    fn raw_value(&mut self, item: &Item, snapshot: &Snapshot) -> Result<Option<Eterm>> {
        let name = match item {
            Item::DictionaryKey(key) => {
                let value = snapshot
                    .dictionary
                    .iter()
                    .find(|(stored, _)| stored == key)
                    .map(|(_, value)| *value);
                return match value {
                    Some(value) => Ok(Some(value)),
                    None => Ok(Some(self.atom("undefined")?)),
                };
            }
            Item::Atom(name) => name.as_str(),
        };
        let value = match name {
            // Handled by the callers (`item_value`/`process_info_1`).
            "registered_name" => return Ok(None),
            "status" => self.atom(snapshot.status)?,
            "message_queue_len" => make_small(snapshot.message_queue_len as i64),
            "messages" => terms::make_list(self, &snapshot.messages)?,
            "links" => terms::make_list(self, &snapshot.links)?,
            "monitors" => self.monitors_value(snapshot)?,
            "monitored_by" => terms::make_list(self, &snapshot.monitored_by)?,
            "dictionary" => self.dictionary_value(snapshot)?,
            "trap_exit" => {
                let atom = bool_atom(snapshot.trap_exit);
                self.atom(atom)?
            }
            "error_handler" => self.atom("error_handler")?,
            "priority" => self.atom(snapshot.priority)?,
            "group_leader" => snapshot.group_leader,
            "heap_size" => make_small(snapshot.heap_size as i64),
            "total_heap_size" => make_small(snapshot.total_heap_size as i64),
            "stack_size" => make_small(snapshot.stack_size as i64),
            "reductions" => make_small(snapshot.reductions as i64),
            "garbage_collection" => self.garbage_collection(snapshot)?,
            "min_heap_size" => make_small(crate::info::effective_policy(self.runtime).min_heap_words as i64),
            "min_bin_vheap_size" => make_small(
                crate::info::effective_policy(self.runtime).min_bin_vheap_words as i64,
            ),
            "fullsweep_after" => {
                make_small(i64::from(crate::info::effective_policy(self.runtime).fullsweep_after))
            }
            "max_heap_size" => {
                let size = crate::info::effective_policy(self.runtime).max_heap_words;
                let flags = self.runtime.config().heap_options().max_heap_flags();
                self.max_heap_size_map(size, flags)?
            }
            // An unchanging M1 runtime default: this runtime's queue is heap-resident.
            "message_queue_data" => self.atom("on_heap")?,
            // An unchanging M1 default: no tracing subsystem exists.
            "trace" => make_small(0),
            // An unchanging M1 default: no `suspend_process`.
            "suspending" => NIL,
            // An unchanging M1 default: no tracing subsystem exists.
            "last_calls" => self.atom("false")?,
            "memory" => make_small(
                ((snapshot.total_heap_size + snapshot.stack_size) * 8 + snapshot.off_heap_bytes)
                    as i64,
            ),
            // A valid oracle item this runtime cannot answer → the caller raises.
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    /// The `monitors` value: `[{process | port, Pid}]`.
    fn monitors_value(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        let mut entries = Vec::with_capacity(snapshot.monitors.len());
        for (is_process, target) in &snapshot.monitors {
            let kind = self.atom(if *is_process { "process" } else { "port" })?;
            entries.push(terms::make_tuple(self, &[kind, *target])?);
        }
        terms::make_list(self, &entries)
    }

    /// The `dictionary` value: the stored `{Key, Value}` pairs, insertion order.
    fn dictionary_value(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        let mut entries = Vec::with_capacity(snapshot.dictionary.len());
        for (key, value) in &snapshot.dictionary {
            entries.push(terms::make_tuple(self, &[*key, *value])?);
        }
        terms::make_list(self, &entries)
    }

    /// The `garbage_collection` value: the oracle's five `{Key, Value}` pairs, in
    /// [`beam_gc::GARBAGE_COLLECTION_KEYS`] order.
    fn garbage_collection(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        let policy = crate::info::effective_policy(self.runtime);
        let info = HeapInfo::new(
            &policy,
            snapshot.minor_gcs,
            snapshot.heap_size,
            snapshot.total_heap_size,
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

    /// `pid`'s observables, or `None` when the process table has no such process.
    fn snapshot(&self, pid: ProcessId) -> Option<Snapshot> {
        let table = self.runtime.processes();
        let process = table.get(pid)?;
        let atoms = self.runtime.code_server.atoms();
        let registered_name = table
            .registered()
            .find(|(_, registered)| *registered == pid)
            .and_then(|(name, _)| atoms.name(name).map(str::to_string));
        Some(Snapshot {
            registered_name,
            status: status_atom(process.state()),
            message_queue_len: process.mailbox().len(),
            messages: process.mailbox().iter().map(|m| m.payload()).collect(),
            links: process.links().iter().map(ProcessId::to_eterm).collect(),
            monitors: process
                .monitors()
                .iter()
                .map(|m| (matches!(m.kind, MonitorKind::Process), m.target.to_eterm()))
                .collect(),
            monitored_by: process
                .monitored_by()
                .iter()
                .map(|m| m.watcher.to_eterm())
                .collect(),
            dictionary: process.dict().entries().collect(),
            trap_exit: process.is_trapping_exit(),
            priority: process.priority().name(),
            group_leader: self.runtime.self_pid().to_eterm(),
            heap_size: process.collector().heap_size_words(),
            total_heap_size: process.collector().total_heap_size_words(),
            stack_size: process.registers().stack().len(),
            reductions: process.reductions(),
            minor_gcs: process.collector().minor_gcs(),
            off_heap_bytes: process.off_heap().overhead_bytes(),
        })
    }

    /// Parses the second argument into the C's three accepted forms.
    fn parse_option(&self, option: Eterm) -> Result<ParsedOption> {
        if beam_term::is_atom(option) {
            return Ok(ParsedOption::Single(self.parse_item(option)?));
        }
        if let Ok(list) = terms::list_elements(option) {
            let mut items = Vec::with_capacity(list.len());
            for element in list {
                items.push(self.parse_item(element)?);
            }
            return Ok(ParsedOption::List(items));
        }
        // Anything else — the `{dictionary, Key}` 2-tuple, an improper list, or a term that
        // is neither atom/list/tuple — goes through `parse_item`, which accepts the
        // `{dictionary, _}` tuple and answers `badarg` for everything else.
        Ok(ParsedOption::Single(self.parse_item(option)?))
    }

    /// Parses one item term (an atom, or the `{dictionary, Key}` 2-tuple).
    fn parse_item(&self, term: Eterm) -> Result<Item> {
        if let Some(name) = self.atom_name(term) {
            if IMPLEMENTED_ITEMS.contains(&name.as_str())
                || UNSUPPORTED_ITEMS.contains(&name.as_str())
            {
                return Ok(Item::Atom(name));
            }
            return Err(BeamError::BadArg);
        }
        if let Ok(2) = terms::tuple_arity(term) {
            let tag = terms::tuple_element(term, 0)?;
            if self.atom_name(tag).as_deref() == Some("dictionary") {
                return Ok(Item::DictionaryKey(terms::tuple_element(term, 1)?));
            }
        }
        Err(BeamError::BadArg)
    }
}

/// The parsed second argument of `erlang:process_info/2`.
enum ParsedOption {
    /// A single item (an atom, or `{dictionary, Key}`).
    Single(Item),
    /// A list of items, in the requested order (duplicates kept).
    List(Vec<Item>),
}

#[cfg(test)]
mod tests;
