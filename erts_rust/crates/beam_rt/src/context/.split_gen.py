#!/usr/bin/env python3
"""One-shot generator for the mod.rs -> submodule split (pure structural move).

Slices the verified line ranges of `mod.rs` into seven cohesive submodules.  Every line
132..NL of the source is assigned to exactly one output file (asserted), and the impl-block
wrapper lines / blank separators are re-synthesised so no two items touch.
"""
import os
import re

SRC = 'erts_rust/crates/beam_rt/src/context/mod.rs'
OUT = 'erts_rust/crates/beam_rt/src/context'

lines = open(SRC).read().split('\n')
if lines[-1] == '':
    lines = lines[:-1]
NL = len(lines)


def get(n):
    return lines[n - 1]


def body(r):
    return '\n'.join(get(i) for i in range(r[0], r[1] + 1))


HDR = '\n'.join(lines[0:19])

# Verified chunk map (start inclusive, end inclusive) per destination.
CHUNKS = {
    'pump': [(132, 140), (141, 154), (155, 170), (171, 204), (1251, 1300), (1301, 1370),
             (1371, 1437), (1438, 1554), (3364, 3502)],
    'rt': [(205, 287), (440, 456), (457, 472), (473, 505), (506, 523), (524, 532), (533, 537),
           (538, 546), (547, 558), (559, 567), (568, 583), (584, 639), (640, 678), (679, 732),
           (1187, 1199), (1230, 1239), (1240, 1250), (1555, 1575)],
    'disp': [(288, 294), (295, 306), (307, 341), (342, 373), (374, 386), (387, 419), (420, 427),
             (428, 438), (1611, 1639), (2094, 2108), (2109, 2118), (2119, 2129), (2130, 2143),
             (2144, 2157), (2158, 2182), (2183, 2196), (2197, 2210), (2211, 2232), (2233, 2263),
             (2264, 2279), (2280, 2296)],
    'send': [(733, 818), (819, 854), (855, 919), (920, 969), (970, 987), (988, 1044),
             (1045, 1077), (1078, 1159), (1160, 1186), (1200, 1213), (1214, 1229), (2534, 2561)],
    'calls': [(1576, 1610), (1640, 2093), (2297, 2390), (2564, 2634), (2635, 2667)],
    'apply': [(2393, 2420), (2422, 2488), (2489, 2533), (3198, 3233), (3234, 3261), (3262, 3326),
              (3327, 3348), (3349, 3363)],
    'exec': [(2670, 3195)],
    'drop': [(439, 439), (2391, 2392), (2421, 2421), (2562, 2562), (2563, 2563), (2668, 2669),
             (3196, 3197)],
}

# Coverage assertion: every line 132..NL exactly once.
cover = {}
for dst, rs in CHUNKS.items():
    for s, e in rs:
        for i in range(s, e + 1):
            assert i not in cover, f'line {i} covered twice ({cover[i]} and {dst})'
            cover[i] = dst
for i in range(132, NL + 1):
    assert i in cover, f'line {i} uncovered'

if os.environ.get('PRINT_ONLY'):
    for dst, rs in CHUNKS.items():
        for s, e in rs:
            print(f'{dst:6s} {s:5d}-{e:5d}  {get(s).strip()[:60]}')
    raise SystemExit(0)


def join(pieces):
    """Join chunks, normalising the blank line between items to exactly one."""
    return '\n\n'.join(p.rstrip('\n') for p in pieces)


def chunk(r):
    return body(r)


def synth_impl(header, ranges):
    inner = '\n\n'.join(chunk(r).rstrip('\n') for r in ranges)
    return header + '\n' + inner + '\n}'


def assemble(parts, fname):
    text = '\n'.join(parts).rstrip('\n') + '\n'
    with open(os.path.join(OUT, fname), 'w') as fh:
        fh.write(text)
    print(f'wrote {fname}: {len(text.splitlines())} lines')


def widen_fns(text, names, indent='    '):
    for n in names:
        pat = r'(?m)^' + re.escape(indent) + r'fn ' + re.escape(n) + r'\('
        text, k = re.subn(pat, indent + 'pub(super) fn ' + n + '(', text)
        assert k >= 1, f'fn {n} not found for widening'
    return text


def widen_fields(text, names, typed=None):
    """Widen `    name: Type,` struct fields to `pub(super) name: Type,`."""
    for n in names:
        pat = r'(?m)^    ' + re.escape(n) + r': '
        text, k = re.subn(pat, '    pub(super) ' + n + ': ', text)
        assert k == 1, f'field {n} widened {k} times'
    return text


DOC_RT = '''
//! The [`RuntimeContext`] type: the runtime borrowed for one run, plus the output sink, the
//! mailbox identity, the caller-module name, the slice's engines and the per-call GC latch
//! and dispatch memo it carries.
//!
//! Everything else about the context lives in this module's siblings — [`crate::context`]
//! maps them.
'''

DOC_DISP = '''
//! The dispatch memo and the `uses_*` hook predicates.
//!
//! A registered BIF call pays a *prologue* before the implementation runs: one
//! [`BifRegistry`](beam_bifs::BifRegistry) lookup plus one evaluation of each hook predicate
//! over `beam_bifs`' `&'static` rosters. [`DispatchMemo`] caches that prologue's answer per
//! `(module, function, arity)` on the context, and [`DispatchHooks`] carries the per-key
//! roster so `call_registered` selects its arm from one value.
'''

DOC_CALLS = '''
//! The registered-BIF call path: the memoized dispatch answer, the bridge selection and the
//! NIF short-circuit.
//!
//! # One context, three dispatch routes
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`] — unless the host can run the callee in-machine, which the
//! [`ExecutionContext::resolve_external_call`] seam reports — and `call_fun`/`call_fun2`
//! through the in-machine [`ExecutionContext::resolve_local_fun`] seam **first**, falling back
//! to the value-returning [`ExecutionContext::apply_fun`]. So [`RuntimeContext::call_bif`]
//! serves, in order:
//!
//! 1. the **runtime-native floor** ([`crate::native`]: `halt`, `display`,
//!    `io:put_chars`, `erlang:apply`, `init:get_argument`);
//! 2. the **`beam_bifs` registry** (the `bif.tab` surface);
//! 3. an **external module call**, loaded on demand from the code path through the
//!    [`CodeServer`](crate::CodeServer) and run with `self` as the context — the
//!    recursion that makes the runtime a real multi-module host.
//!
//! # The shared atom table
//!
//! A [`BifContext`] takes *one* [`AtomTable`](beam_term::AtomTable). The engine resolves
//! atoms — including the ones inside `Module` code — against the **loaded module's**
//! table (`beam_engine`'s `cmp(.., &module.atom_table)`). Because every module the
//! [`CodeServer`](crate::CodeServer) loads is interned into the runtime's **one shared**
//! table, handing the BIFs that same table is exactly right: an atom a BIF builds is an
//! index the engine can compare and the printer can name, for **every** loaded module.
//!
//! # Interning new atoms (the predicted-index commit)
//!
//! Some BIFs (`binary_to_term/1`, `list_to_atom/1`, …) must be able to add a **new** atom
//! to that same shared table, but the [`BifContext`] only borrows it *immutably* for index
//! resolution. Since this crate is `#![deny(unsafe_code)]`, the interner cannot hold a
//! second, mutable borrow of the one table. The [`AtomInterner`](beam_bifs::AtomInterner)
//! hook solves this without aliasing: [`PendingInterner`] records the names a BIF interns
//! during a call and **predicts** each index as `base + <names already recorded>` (interning
//! is append-only and the runtime is single-threaded, so the prediction is exact); after the
//! BIF returns — releasing the table borrow — [`RuntimeContext::call_registered`] commits the
//! recorded names into the shared table in the same order, so each lands at exactly the
//! predicted index. A name seen twice in one call is deduplicated and gets the same index.
'''

DOC_SEND = '''
//! The send/deliver family: the runtime's own copy-into-the-receiver-heap path.
//!
//! Every delivery resolves its **source off-heap table** through one builder
//! ([`RuntimeContext::send_source`]) and routes a copy the receiver's heap cannot afford to
//! the heap-fragment fallback, so a genuine exhaustion is the documented `error:system_limit`
//! rather than a lost message.
'''

DOC_APPLY = '''
//! Fun application and the callee-admission predicates.
//!
//! [`LocalFunTarget`] is the launch target of a `.beam` **local** (`make_fun3`) closure, and
//! [`RuntimeContext::local_fun_target`] resolves one; [`RuntimeContext::apply_non_local_fun`]
//! applies the two fun kinds that have no local label. The predicates
//! ([`callee_observes_its_call_site`] and its helpers) decide whether the engine's
//! cross-module call seam may enter a callee **in-machine** or must keep it on the
//! synchronous call route.
'''

DOC_PUMP = '''
//! The nested-run pump: running a module export as a **scheduled process**.
//!
//! [`RuntimeContext::run_export`] drives one export on a fresh [`Machine`], and its blocking
//! twins ([`RuntimeContext::run_export_blocking`] / [`RuntimeContext::run_label_blocking`])
//! pump the scheduler while a cross-module callee is suspended in a `receive`, so the callee
//! **suspends and resumes** instead of folding into the typed [`beam_engine::blocked`] gap.
//! [`pump_until_returned`] is the shared bounded loop and [`BLOCKING_PUMP_DEADLINE`] its
//! wall-clock safety cap.
'''

DOC_EXEC = '''
//! The single `impl ExecutionContext for RuntimeContext<'_>` — the engine's seam.
//!
//! # The process-heap collector
//!
//! [`ensure_heap`](ExecutionContext::ensure_heap) and
//! [`alloc_words`](ExecutionContext::alloc_words) are served from the running **scheduled**
//! process's own young heap, collected at the heap-admission point — see
//! [`crate::context::gc`] for the gate, the float guard and the root-set hand-off. Everything
//! outside a scheduler slice still runs on the runtime's pointer-stable
//! [`MinimalContext`](beam_engine::MinimalContext) arena, byte for byte as before.
//!
//! All trait methods live in this one block: Rust forbids a second
//! `impl ExecutionContext for RuntimeContext<'_>` (E0119), so the seam is split **by concern
//! inside the block**, with each method delegating to an inherent helper in the sibling module
//! that owns that concern.
'''

# ---------------------------------------------------------------- runtime_context.rs
rt = CHUNKS['rt']
struct_chunk = join([chunk(rt[0])])
methods = join([c.rstrip('\n') for c in (chunk(r) for r in rt[1:])])
rt_body = struct_chunk + '\n\n' + 'impl<\'a> RuntimeContext<\'a> {\n' + methods + '\n}'
rt_body = widen_fields(rt_body, ['mailbox_pid', 'mailbox_table', 'may_pump_scheduler',
                                'current_module', 'engines', 'gc_armed', 'dispatch_memo'])

# ---------------------------------------------------------------- dispatch.rs
disp = CHUNKS['disp']
top = join([chunk(r) for r in disp if r[0] < 2000])
meth = [r for r in disp if r[0] >= 2000] + [(1611, 1639)]
meth.sort()
disp_body = top + '\n\n' + synth_impl('impl<\'a> RuntimeContext<\'a> {', meth)
disp_body = re.sub(r'(?m)^const DISPATCH_MEMO_SLOTS: usize = 8;',
                   'pub(super) const DISPATCH_MEMO_SLOTS: usize = 8;', disp_body)
disp_body = re.sub(r'(?m)^fn dispatch_slot\(', 'pub(super) fn dispatch_slot(', disp_body)
for s in ['DispatchHooks', 'DispatchAnswer', 'DispatchMemo', 'DispatchEntry']:
    pat = r'(?m)^struct ' + s + r' \{'
    disp_body, k = re.subn(pat, 'pub(super) struct ' + s + ' {', disp_body)
    assert k == 1, s
disp_body = widen_fields(disp_body, ['dict', 'process_hook', 'nif_loader', 'record_source',
                                     'send_hook', 'module_info', 'persistent_term',
                                     'runtime_hooks', 'fun_identity', 'memory_hook'])
disp_body = widen_fields(disp_body, ['bif', 'hooks'])
disp_body = widen_fields(disp_body, ['slots'])
disp_body = widen_fields(disp_body, ['module', 'function', 'arity', 'answer'])
disp_body = widen_fns(disp_body, ['of'])
disp_body = widen_fns(disp_body, ['dispatch', 'uses_dict', 'uses_process_hook',
                                  'reentrancy_capable', 'uses_nif_loader', 'uses_record_source',
                                  'uses_send_hook', 'uses_module_info', 'uses_persistent_term',
                                  'persistent_term_stores', 'uses_runtime_hooks',
                                  'uses_memory_hook', 'uses_fun_identity'])

# ---------------------------------------------------------------- calls.rs
calls = CHUNKS['calls']
c1 = [r for r in calls if r[0] < 2100] + [r for r in calls if r[0] == 2297]
c1.sort()
c2 = [r for r in calls if r[0] >= 2500]
calls_body = (synth_impl('impl<\'a> RuntimeContext<\'a> {', c1)
              + '\n\n' + synth_impl('impl RuntimeContext<\'_> {', c2))
calls_body = widen_fns(calls_body, ['call_registered', 'try_call_nif', 'call_bif_routed',
                                    'open_gate_for_foreign_result'])

# ---------------------------------------------------------------- send.rs
send_body = synth_impl('impl<\'a> RuntimeContext<\'a> {', CHUNKS['send'])
send_body = widen_fns(send_body, ['send_source', 'deliver_message_with_off_heap_inner',
                                  'deliver_into_mailbox', 'delivery_collection_is_unsound',
                                  'send_fragmented_message', 'mailbox_parts'])

# ---------------------------------------------------------------- apply.rs
app = CHUNKS['apply']
app_struct = join([chunk(r) for r in app if r[0] < 2400])
app_meth = [r for r in app if 2400 <= r[0] < 3000]
app_fns = join([chunk(r) for r in app if r[0] >= 3000])
app_body = (app_struct + '\n\n' + synth_impl('impl RuntimeContext<\'_> {', app_meth)
            + '\n\n' + app_fns)
app_body = re.sub(r'(?m)^enum CallSiteReadingImports \{',
                  'pub(super) enum CallSiteReadingImports {', app_body)
app_body = widen_fns(app_body, ['call_site_reading_imports', 'function_calls_a_fun_image_bif',
                                'import_is_a_fun_image_bif'], indent='')

# ---------------------------------------------------------------- pump.rs
pmp = CHUNKS['pump']
pmp_top = join([chunk(r) for r in pmp if r[0] < 1000])
pmp_impl = [r for r in pmp if 1000 <= r[0] < 2000]
pmp_test = join([chunk(r) for r in pmp if r[0] >= 3000])
pump_body = (pmp_top + '\n\n' + synth_impl('impl<\'a> RuntimeContext<\'a> {', pmp_impl)
             + '\n\n' + pmp_test)
pump_body = re.sub(r'(?m)^const BLOCKING_PUMP_DEADLINE: Duration',
                   'pub(super) const BLOCKING_PUMP_DEADLINE: Duration', pump_body)
pump_body = re.sub(r'(?m)^fn pump_until_returned<F>\(',
                   'pub(super) fn pump_until_returned<F>(', pump_body)
pump_body = widen_fns(pump_body, ['run_export_blocking', 'run_label_blocking', 'pump_once'])
pump_body = pump_body.replace(
    '    use super::*;\n'
    '    use crate::context::test_support::{compile_fixture, find_on_path, resolve_erlc, skip_loudly};\n'
    '    use beam_term::make_small;\n',
    '    use super::super::RuntimeContext;\n'
    '    use crate::context::test_support::{compile_fixture, find_on_path, resolve_erlc, skip_loudly};\n'
    '    use crate::runtime::Runtime;\n'
    '    use beam_process::ProcessTable;\n'
    '    use beam_term::make_small;\n')

# ---------------------------------------------------------------- execution.rs
exec_body = join([chunk(r) for r in CHUNKS['exec']])

for name, b in [('rt', rt_body), ('disp', disp_body), ('calls', calls_body), ('send', send_body),
                ('apply', app_body), ('pump', pump_body), ('exec', exec_body)]:
    assert 'fn ' in b or 'impl' in b, name

# ---------------------------------------------------------------- shared import block
IMPORTS = '''
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_abi::nif::loader::NifCallError;
use beam_bifs::{BifContext, BifFn, OutputSink};
use beam_common::{AllocKind, BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::machine::WaitDeadline;
use beam_engine::{terms, ExecutionContext, Machine, RunOutcome};
use beam_loader::{Atom, Function, ImportRef, Instr, Label, Module};
use beam_message::{CopyContext, Message, OffHeapTable, Queue};
use beam_process::{MonitorRef, ProcessId, ProcessTable};
use beam_term::Eterm;

use crate::error;
use crate::mailbox::MailboxState;
use crate::nif::{NifHostImpl, RuntimeNifServices};
use crate::port::RuntimePorts;
use crate::runner::RunnerState;
use crate::runtime::Runtime;

use super::alias;
use super::bridges::{
    descriptor_of, DictBridge, FunBridge, NifBridge, NodeIdentity, RecordBridge, SendBridge,
    SharedSink,
};
use super::dispatch::{DispatchAnswer, DispatchHooks, DispatchMemo, dispatch_slot};
use super::fun_identity::FunIdentityBridge;
use super::gc_walk::process_roots_are_unscanned;
use super::memory_hook::RuntimeMemory;
use super::module_info::ModuleInfoBridge;
use super::persistent_term::{PersistentTermBridge, PersistentTermStore};
use super::process_bridge::ProcessBridge;
use super::runtime_context::RuntimeContext;
use super::runtime_hooks::RuntimeHooksBridge;
'''

assemble([HDR, DOC_RT, IMPORTS, rt_body], 'runtime_context.rs')
assemble([HDR, DOC_DISP, IMPORTS, disp_body], 'dispatch.rs')
assemble([HDR, DOC_CALLS, IMPORTS, calls_body], 'calls.rs')
assemble([HDR, DOC_SEND, IMPORTS, send_body], 'send.rs')
assemble([HDR, DOC_APPLY, IMPORTS, app_body], 'apply.rs')
assemble([HDR, DOC_PUMP, IMPORTS, pump_body], 'pump.rs')
assemble([HDR, DOC_EXEC, IMPORTS, exec_body], 'execution.rs')
print('done')
