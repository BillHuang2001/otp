// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The runtime-native BIF floor: the BIFs `beam_bifs` deliberately does not provide.
//!
//! `beam_bifs` owns the `bif.tab` surface — arithmetic, comparison, type tests, term
//! ops, `lists`/`maps`/`binary`, `io:format`, `ets`, … — but a handful of "BIFs" are
//! really *runtime* operations that need the host's own state and are therefore served
//! here, **before** the registry is consulted:
//!
//! | Call | Why it is runtime-native |
//! |---|---|
//! | `erlang:halt/0,1,2` | terminates the runtime: it must travel out of the running code as a raise and become the process exit status |
//! | `erl_ddll:loaded_drivers/0`, `erl_ddll:try_load/3`, `erl_ddll:try_unload/2`, `erl_ddll:info/2` | the loaded-driver **list** an `open_port({spawn_driver, Name}, …)` resolves the name against: it is the runtime's own `dlopen`/port registry (`crate::port::driver`), not `beam_bifs`'s separate one |
//! | `erlang:display/1` | writes to the *process's* output sink |
//! | `io:put_chars/1,2`, `io:nl/0,1`, `io:fwrite/1,2,3`, `io:write/1,2`, `io:format/3` | the `io` **device** surface: the `io.erl` request wrappers' own `{io_request, …}` round trip against the caller's group leader (itself) answers `calling_self`, so the runtime resolves the device and writes to the process's output sink or its standard error itself (see [`io`]) |
//! | `erlang:apply/2,3` | re-enters the host's own dispatch (BIF **or** load-on-demand module call) |
//! | `erlang:system_info/1` | reads/renders the runtime introspection surface (contract A.8) on the runtime's own scratch heap |
//! | `erlang:process_info/2` | reads a **process's** heap/GC observables out of this runtime's own process table (`beam_gc`'s `HeapInfo`; contract A.8) |
//! | `erlang:process_flag(monitor_nodes \| {monitor_nodes, Opts}, Bool)` | the undocumented node-monitor flag `net_kernel:monitor_nodes/1,2` sets: the `beam_bifs` `ProcessHook` seam takes an **atom** flag, so the tuple/option-map form is served here from the runtime's own per-pid monitor-node store (contract A.5/A.9, `src/native/process.rs`) |
//! | `erlang:hibernate/0` | an OTP-30 **instruction** (`emu/ops.tab:896-898` rewrites `call_ext erlang:hibernate/0` to `i_hibernate`), not a `bif.tab` BIF; this runtime's loader performs no such transform, so the call reaches the floor, which models "hibernate" as **continue** (the C's `erts_hibernate` only shrinks the heap and clears the ACTIVE flag; the caller's own following `receive` blocks) — `src/native/process.rs` |
//! | `init:get_argument/1` | reads the parsed command line (contract A.6, `RuntimeConfig::init_args`) |
//! | `init:get_plain_arguments/0` | the (always empty) plain-argument list — the CLI models no plain arguments |
//! | `init:get_arguments/0` | the whole command line, in order, from `RuntimeConfig::init_args`, preceded by the emulator's own automatic flags (`root`/`bindir`/`progname`/`home`) |
//! | `init:stop/0,1` | terminates the **node**: records the exit status (or raises the crash-dump slogan / `badarg`) and answers `ok` without interrupting the caller |
//! | `init:restart/0` | reboots the node **in place** (resets the code path to its boot default, re-runs the boot sequence and the loaded boot script) and answers `ok` (`init.erl:492-518,937-954`) |
//! | `init:boot/1` | the boot **entry point**: parses the `-boot <Script>` flag out of the byte-list argument, resolves it exactly as the CLI's `-boot FILE` does and executes the script (`init.erl:585-597`) |
//! | `erlang:is_alive/0`, `erlang:get_cookie/0`, `erlang:set_cookie/2` | read/replace this node's own identity (the name, the cookie) and its liveness — the distribution state no BIF module can reach (contract A.5, `src/native/node.rs`) |
//! | `erlang:nodes/0,1,2` | read this runtime's own **link table** (the established outbound distribution links), not a registry value (contract A.5, `src/native/net.rs`) |
//! | `net_adm:ping/1`, `net_kernel:connect_node/1` | **drive** that link table: the outbound `epmd` lookup + `'N'` client handshake (`Runtime::dist_connect`) is host socket work (contract A.5, `src/native/net.rs`) |
//! | `erl_distribution:start_link/0,1` | the Erlang-level distribution **supervisor** `kernel_sup` starts as its `net_sup` child: answered `ignore` because this runtime's distribution is the native [`beam_dist`](crate::dist)/`DistNode`, not the `inet_tcp`/`prim_inet` stack that supervisor would run (contract A.5, `src/native/net.rs`) |
//! | `os:cmd/1` | runs a command through `/bin/sh` and returns its stdout — the host's own process spawn |
//! | `code:get_path/0`, `code:all_loaded/0`, `code:which/1`, `code:get_object_code/1`, `code:is_loaded/1`, `code:is_sticky/1,2`, `code:ensure_loaded/1`, `code:load_file/1`, `code:lib_dir/0,1,2`, `code:priv_dir/1`, `code:root_dir/0` | read the runtime's own [`CodeServer`](crate::CodeServer) state (the code path, the loaded/preloaded module set, the installation) — and, for `get_object_code/1`, its `.beam` bytes |
//! | `application:get_env/1,2,3`, `application:get_all_env/1`, `application:get_key/2`, `application:loaded_applications/0`, `application:which_applications/0` | read the runtime's own **application-config view** — the `.app` resource files of the OTP applications its code path resolves |
//! | `prim_file:get_cwd/0,1`, `prim_file:read_file/1`, `prim_file:open/2`, `prim_file:read/2`, `prim_file:write/2`, `prim_file:close/1`, `prim_file:position/2` | the file-system **primitives** `file.erl` delegates to: they are the host's own `getcwd`/`open`/`read`/`write`/`close`/`lseek` (`std::env`/`std::fs`), which only the host can perform |
//!
//! The `init`/`code`/`application` calls are **preloaded-module surface** (contract
//! A.7): the oracle serves them from `init.beam`/`code.beam`/`application.beam`, but
//! those modules are written against the process/message machinery Milestone-1 does not
//! run (for `application`, that is the `application_controller`/`application_master`
//! process pair plus a group leader), so this runtime answers them directly from its own
//! state instead of loading a module it cannot execute. `os:cmd/1` is the same story for
//! the spawn **driver** (contract A.4): the port machinery is a later stage, but the host
//! is perfectly able to spawn a shell.
//!
//! Every entry returns `Option<Result<Eterm>>`: `None` means "not a native BIF, fall
//! through to the `beam_bifs` registry", `Some(result)` is the answer (which may be an
//! error). Nothing is re-implemented that the registry already owns, and terms are
//! built through [`beam_engine::terms`] (`beam_rt` stays `unsafe`-free) on the heap the
//! engine's `alloc_words` seam exposes.
//!
//! # The `halt` convention
//!
//! `halt` does not return a value; it raises an `exit`-class exception that terminates the
//! runtime, and the shape of the raise encodes *how*:
//!
//! | Call | Reason atom | CLI action |
//! |---|---|---|
//! | `halt(Status)` / `halt(Status, Options)` | `beam_rt_halt:<status>` (see [`crate::error::halt`]) | exit with `<status>` |
//! | `halt(abort)` / `halt(abort, Options)` | `beam_rt_abort` ([`crate::error::abort`]) | write the crash dump, then `SIGABRT` |
//! | `halt("Slogan")` / `halt([], Options)` | `beam_rt_dump:<slogan>` ([`crate::error::dump`]) | print the slogan, write the crash dump, exit 1 |
//!
//! The runtime also records the status ([`Runtime::halt_status`](crate::Runtime::halt_status))
//! so a host that catches the raise can still observe it, and the CLI turns it into the
//! process exit code (or the signal).
//!
//! # Module layout
//!
//! The floor is split by responsibility: the `erlang`-core items and the shared helpers live
//! in this module; the `init:*` entries in [`init`], the `code:*` entries in [`code`], the
//! `erlang:*` process/registry/reference entries in [`process`], the `application:*`
//! entries in [`application`], the `prim_file:*` primitives in [`prim`], the `erl_ddll:*`
//! loaded-driver-list entries in [`ddll`], the distribution **identity** in [`node`] and the
//! distribution **node set + connector** (`erlang:nodes/0,1,2`, `net_adm:ping/1`,
//! `net_kernel:connect_node/1`, `erl_distribution:start_link/0,1`) in [`net`]. The submodules
//! reach the shared helpers through `super` (they are `pub(super)`).

mod application;
mod code;
mod ddll;
mod erts_internal;
pub(crate) mod file_server;
mod group_leader;
mod init;
mod io;
mod loader;
pub(crate) mod memory;
mod net;
mod node;
pub(crate) mod port;
mod prim;
mod prim_file;
pub(crate) mod prim_tty;
mod process;
#[cfg(test)]
pub(crate) mod test_support;

use std::process::Command;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext};
use beam_term::{atom_val, is_atom, is_small, make_atom, make_small, signed_value, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::error;
use crate::format;

/// The module namespaces [`RuntimeContext::try_native`] serves — the floor's own dispatch.
///
/// Every one of them is a module the code server can *also* load as a `.beam` (`erlang`,
/// `io`, `init`, `os`, `code`, `application`, `erts_internal`, `prim_file`, `erl_ddll`,
/// `net_adm`, `net_kernel`, `erl_distribution`), so the cross-module-call seam
/// ([`RuntimeContext::resolve_external_call`]) must answer `Ok(None)` for all of them:
/// entering the module body would run it *instead of* the host implementation. The list is
/// written next to the `try_native` match arms it mirrors, and
/// [`tests::every_floor_module_is_a_real_try_native_arm`] pins it against that source so it
/// cannot silently drift out of the host's dispatch.
///
/// Membership here only means "a namespace `try_native` serves"; whether the *whole*
/// namespace is refused or only [`floor_keys`]' enumerated keys is decided there. The
/// `erl_distribution` entry is key-granular (the floor serves exactly `start_link/0,1`), so
/// the rest of the module stays enterable — see [`floor_keys`].
pub(crate) const FLOOR_MODULES: [&str; 14] = [
    "application",
    "code",
    "erl_ddll",
    "erl_distribution",
    "erl_prim_loader",
    "erlang",
    "erts_internal",
    "init",
    "io",
    "net_adm",
    "net_kernel",
    "os",
    "prim_file",
    "prim_tty",
];

/// Whether [`RuntimeContext::resolve_external_call`] must refuse to enter
/// `(module, function, arity)` **in-machine** because the runtime-native floor owns that
/// EXACT key.
///
/// The module-level rule ([`FLOOR_MODULES`]) is right for a namespace whose every name is
/// either a floor arm or a `beam_bifs` registry BIF: entering the module body would run it
/// *instead of* the host implementation — a wrong result. A namespace with an enumerated
/// [`floor_keys`] list is instead **key-granular**: only those keys are refused and every
/// other name is an ordinary `.beam` body.
///
/// `io` is the sharpest case. The floor serves only the device surface
/// (`io:put_chars/1,2`, `io:nl/0,1`, `io:fwrite/1,2,3`, `io:write/1,2` and `io:format/3`)
/// for that namespace ([`floor_keys`]'s `io` arm); every *other* `io:*`
/// (`io:getopts/1`, `io:setopts/2`, `io:request/2`, `io:scan_erl_form/4`, …) is an ordinary
/// body in `lib/stdlib/src/io.erl`, and **refusing** it here routes it onto the synchronous
/// non-pumping [`RuntimeContext::call_bif`] path instead.
///
/// That refusal is the defect: a **scheduled** process is a slice
/// ([`RuntimeContext::for_process`], `may_pump_scheduler() == false`), so
/// `run_export_blocking`'s `pump_once` returns `Ok(None)` and the callee's blocking
/// `receive` on the io-device request/reply folds into the typed `blocked()` capability gap
/// (`unsupported: opcode: run: process blocked (no message available)`, exit 3) instead of
/// suspending the process. Entering the same body **in-machine** (`Ok(Some(label))`) lets
/// that `receive` suspend the process through the ordinary `RunOutcome::Wait` path, which
/// the scheduler resumes once the io device replies — exactly how `file:*` (a non-floor
/// module) already works.
///
/// This is the contract A.9 `compile:file/2` blocker: `epp:open/1`'s server calls
/// `io:getopts/1` and `epp:parse_file/2` calls `io:scan_erl_form/4`, both of which stopped
/// a scheduled process here.
///
/// The device surface ([`floor_keys`]'s `io` arm) must therefore stay **refused** (the floor
/// owns it). `io:format/1,2` and `io:printable_range/0` are `beam_bifs` registry BIFs and are
/// still excluded, but by the registry check that `resolve_external_call` applies *after* this
/// one.
///
/// [`try_native`]: RuntimeContext::try_native
/// [`RuntimeContext::resolve_external_call`]: crate::context::RuntimeContext
/// [`RuntimeContext::call_bif`]: crate::context::RuntimeContext
/// [`RuntimeContext::for_process`]: crate::context::RuntimeContext
pub(crate) fn floor_shadows(module: &str, function: &str, arity: u32) -> bool {
    if let Some(keys) = floor_keys(module) {
        return keys.contains(&(function, arity));
    }
    FLOOR_MODULES.contains(&module)
}

/// The **explicitly enumerated** keys of a floor namespace — the ones whose `try_native` arms
/// name a single function and arity each, so the host owns exactly those keys and every other
/// name in the namespace is a `.beam` body.
///
/// `None` for a namespace [`try_native`] serves through a **wildcard** arm
/// (`erts_internal`, `prim_file`, `erl_ddll`) or for one whose arms are too numerous to
/// mirror here (`erlang`); those keep the module-wide exclusion.
fn floor_keys(module: &str) -> Option<&'static [(&'static str, u32)]> {
    match module {
        // The `io` **device** surface: `put_chars/1,2` plus the `io.erl` request wrappers
        // `nl/0,1`, `fwrite/1,2,3`, `write/1,2` and `format/3`, which this runtime serves
        // natively because `io.erl`'s own `{io_request, …}`/`{io_reply, …}` round trip
        // against the caller's group leader (itself) answers `calling_self` (see [`io`]).
        // Every arm resolves its device through the one model in that module.
        // Every *other* `io:*` (`io:getopts/1`, `io:setopts/2`, `io:request/2`,
        // `io:scan_erl_form/4`, `io:format/1,2`, `io:printable_range/0`, …) is an ordinary
        // body in `lib/stdlib/src/io.erl` or a `beam_bifs` registry BIF.
        "io" => Some(&[
            ("put_chars", 1),
            ("put_chars", 2),
            ("nl", 0),
            ("nl", 1),
            ("fwrite", 1),
            ("fwrite", 2),
            ("fwrite", 3),
            ("write", 1),
            ("write", 2),
            ("format", 3),
        ]),
        // `prim_tty` is a NIF library served whole from the floor: its `NIF_KEYS` list is
        // exactly the `-nifs` surface of `lib/kernel/src/prim_tty.erl`, so the stubs are
        // refused (→ `call_bif` → `try_native`) while the module's Erlang wrappers are
        // ordinary `.beam` bodies.
        "prim_tty" => Some(&prim_tty::NIF_KEYS),
        // `application` is partly `lib/kernel/src/application.erl` bodies (`set_env/3,4`,
        // `load/1`, `start_boot/2`, `unset_env/2,3`, …), each a thin wrapper that delegates
        // to `application_controller` — the mirror of the `io` case.
        "application" => Some(&[
            ("get_env", 1),
            ("get_env", 2),
            ("get_env", 3),
            ("get_all_env", 1),
            ("get_key", 1),
            ("get_key", 2),
            ("get_application", 0),
            ("get_application", 1),
            ("get_all_key", 1),
            ("loaded_applications", 0),
            ("which_applications", 0),
        ]),
        // `code` is the same shape: the floor answers the *queries* below **and** the
        // path-mutating entries (`add_patha/1,2`, `add_pathz/1,2`, `add_path/1,2`,
        // `del_path/1`). Entering the `lib/kernel/src/code.erl` bodies instead would reach the
        // *live* `code_server` process, whose path store is a **second, incoherent** one: this
        // runtime's `code:get_path/0`, `code:load_file/1`, `code:which/1`,
        // `code:get_object_code/1`, `code:ensure_loaded/1` and `code:where_is_file/1` all read
        // [`CodeServer`](crate::CodeServer)'s path, so a directory added through the module
        // body landed only in the code server's own copy —
        // `code:add_patha(Dir), code:load_file(M)` answered `{error,nofile}` while
        // `lists:member(Dir, code:get_path())` (answered by that same process) said `true`.
        // The floor owns the **one** authoritative store instead. Every *other* `code:*`
        // (`stick_dir/1`, `set_path/1,2`, `add_paths/1`, `del_paths/1`, `unstick_dir/1`, …)
        // stays an ordinary `lib/kernel/src/code.erl` body.
        // `init` is the same shape as `io`/`application`/`code`: the floor answers the
        // queries below and every other `init:*` is a `lib/kernel/src/init.erl` body
        // (`code_path_choice/0`, …). Refusing it wholesale routed `code_server`'s
        // `init:code_path_choice/0` onto the synchronous non-pumping `call_bif` path, where
        // the body's own `init ! Request` + `receive` folded into the typed `blocked()` gap.
        "init" => Some(&[
            ("get_argument", 1),
            ("get_plain_arguments", 0),
            ("get_arguments", 0),
            ("stop", 0),
            ("stop", 1),
            ("restart", 0),
            ("boot", 1),
            ("get_status", 0),
            ("script_id", 0),
            ("fetch_loaded", 0),
            ("code_path_choice", 0),
        ]),
        "code" => Some(&[
            ("get_path", 0),
            ("all_loaded", 0),
            ("which", 1),
            ("get_object_code", 1),
            ("is_loaded", 1),
            ("is_sticky", 1),
            ("is_sticky", 2),
            ("ensure_loaded", 1),
            ("load_file", 1),
            ("lib_dir", 0),
            ("lib_dir", 1),
            ("lib_dir", 2),
            ("root_dir", 0),
            ("get_mode", 0),
            ("priv_dir", 1),
            ("where_is_file", 1),
            // The **path-mutating** half of `code` — see the note on [`floor_keys`]'s `code`
            // arm. The floor owns these so the path this runtime *searches* and the path
            // `code:get_path/0` *reports* are the one [`CodeServer`](crate::CodeServer) store;
            // entering the `code.erl` bodies would talk to the live `code_server` process and
            // its separate copy. The bodies live in [`native::code::path`].
            ("add_patha", 1),
            ("add_patha", 2),
            ("add_pathz", 1),
            ("add_pathz", 2),
            ("add_path", 1),
            ("add_path", 2),
            ("del_path", 1),
        ]),
        // The Erlang-level distribution supervisor entry. `kernel_sup`'s `net_sup` child is
        // `{erl_distribution, start_link, []}` (`lib/kernel/src/kernel.erl:264`) and
        // `start_link/1` is the same entry `erl_distribution:start/1` invokes from
        // `net_kernel:start/1`; both start a supervisor whose children are the Erlang socket
        // stack (`protocol_childspecs`, `erl_epmd`, `auth`, `net_kernel`) this runtime has no
        // `prim_inet` driver for — its distribution is the native `beam_dist`/`DistNode` the
        // CLI already bound. So the floor owns exactly those two keys and answers the atom
        // `ignore` (nothing to start), which is the same value the oracle's own
        // `erl_distribution:start_link/0` returns when no `-sname`/`-name` argument is
        // present. Every *other* `erl_distribution:*` (`init/1`, `start/1`, `stop/0`) stays an
        // ordinary `lib/kernel/src/erl_distribution.erl` body, so the namespace is
        // key-granular. See [`crate::native::net`].
        "erl_distribution" => Some(&[("start_link", 0), ("start_link", 1)]),
        _ => None,
    }
}

impl RuntimeContext<'_> {
    /// The runtime-native BIF floor, consulted **before** the `beam_bifs` registry.
    ///
    /// Returns `None` when `(module, function, arity)` is not a native BIF — the
    /// caller then dispatches through the registry (or loads the module).
    pub(crate) fn try_native(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        match (module, function, arity) {
            ("erlang", "halt", 0) => Some(Err(self.record_halt(0))),
            ("erlang", "halt", 1) => Some(self.halt_1(args)),
            ("erlang", "halt", 2) => Some(self.halt_2(args)),
            ("erlang", "display", 1) => Some(self.display(args)),
            ("io", "put_chars", 1) => Some(self.io_put_chars(args, 1)),
            ("io", "put_chars", 2) => Some(self.io_put_chars(args, 2)),
            // The rest of the `io` **device** surface — the `io.erl` wrappers over
            // `o_request/2`/`execute_request/3`, which this runtime cannot complete against
            // itself (its own root process's group leader is itself, so `io.erl` answers
            // `{ErrorTag, calling_self}`). Serving them here removes `io.erl` from the path
            // and answers the device question natively; the rendering still goes through the
            // registered `io_lib:format/2` BIF. See [`io`].
            ("io", "nl", 0) => Some(self.io_nl(args, 0)),
            ("io", "nl", 1) => Some(self.io_nl(args, 1)),
            ("io", "fwrite", 1) => Some(self.io_fwrite(args, 1)),
            ("io", "fwrite", 2) => Some(self.io_fwrite(args, 2)),
            ("io", "fwrite", 3) => Some(self.io_fwrite(args, 3)),
            ("io", "write", 1) => Some(self.io_write(args, 1)),
            ("io", "write", 2) => Some(self.io_write(args, 2)),
            ("io", "format", 3) => Some(self.io_format_3(args)),
            ("erlang", "apply", 2) => Some(self.apply_dynamic(args, false)),
            ("erlang", "apply", 3) => Some(self.apply_dynamic(args, true)),
            ("erlang", "system_info", 1) => Some(self.system_info_1(args)),
            // `process_info/1,2` stay on the floor: they answer from *this* runtime's
            // process tables through the shared `crate::native::memory` implementation
            // (which the `ProcessHook` bridge also calls), and they serve the
            // `{dictionary, Key}` item form the `beam_bifs` seam cannot express.
            ("erlang", "process_info", 2) => Some(self.process_info_2(args)),
            ("erlang", "process_info", 1) => Some(self.process_info_1(args)),
            ("init", "get_argument", 1) => Some(self.get_argument(args)),
            ("init", "get_plain_arguments", 0) => Some(self.get_plain_arguments()),
            ("init", "get_arguments", 0) => Some(self.get_arguments()),
            ("init", "stop", 0) => Some(self.stop_0()),
            ("init", "stop", 1) => Some(self.stop_1(args)),
            ("init", "restart", 0) => Some(self.restart_0()),
            ("init", "boot", 1) => Some(self.boot_1(args)),
            ("init", "get_status", 0) => Some(self.get_status()),
            ("init", "script_id", 0) => Some(self.script_id_0()),
            ("init", "fetch_loaded", 0) => Some(self.fetch_loaded()),
            ("init", "code_path_choice", 0) => Some(self.code_path_choice()),
            ("os", "cmd", 1) => Some(self.os_cmd(args)),
            // The port surface the `PortHook` seam cannot express: `port_command/2,3` (the
            // port's *input* direction) and `port_control/3` (the driver's `control` callback) —
            // see [`port`] — plus the closed-port half of `port_info/1,2`. Every other form of
            // those three names falls through to the registry (`port_native` answers `None`).
            ("erlang", "port_command" | "port_control" | "port_info", 1 | 2 | 3) => {
                self.port_native(function, arity, args)
            }
            ("code", "get_path", 0) => Some(self.code_get_path()),
            ("code", "all_loaded", 0) => Some(self.code_all_loaded()),
            ("code", "which", 1) => Some(self.code_which(args)),
            ("code", "get_object_code", 1) => Some(self.code_get_object_code(args)),
            ("code", "is_loaded", 1) => Some(self.code_is_loaded(args)),
            ("code", "is_sticky", 1) => Some(self.code_is_sticky_1(args)),
            ("code", "is_sticky", 2) => Some(self.code_is_sticky_2()),
            ("code", "ensure_loaded", 1) => Some(self.code_ensure_loaded(args)),
            ("code", "ensure_modules_loaded", 1) => Some(self.code_ensure_modules_loaded(args)),
            ("code", "load_file", 1) => Some(self.code_load_file(args)),
            ("code", "lib_dir", 0) => Some(self.code_lib_dir_0()),
            ("code", "lib_dir", 1) => Some(self.code_lib_dir_1(args)),
            ("code", "root_dir", 0) => Some(self.code_root_dir()),
            ("code", "get_mode", 0) => Some(self.code_get_mode()),
            ("code", "lib_dir", 2) => Some(self.code_lib_dir_2(args)),
            ("code", "priv_dir", 1) => Some(self.code_priv_dir_1(args)),
            // `where_is_file/1` is the code-server search `application_controller`
            // uses to locate an application resource such as `kernel.app`; it is
            // path-based and needs no loaded module, so the floor serves it (see
            // [`code`](crate::native::code)).
            ("code", "where_is_file", 1) => Some(self.code_where_is_file(args)),
            // The **path-mutating** `code:*` entries (see [`floor_keys`]): the floor owns them
            // so this runtime's [`CodeServer`](crate::CodeServer) is the ONE code path — both
            // the store `code:get_path/0` reports and the one `code:load_file/1`,
            // `code:which/1` and every module lookup search. `add_path/1,2` folds into
            // `add_pathz` exactly as `code.erl:1075-1077` does.
            ("code", "add_patha", 1 | 2) => Some(self.code_add_patha(args)),
            ("code", "add_pathz", 1 | 2) => Some(self.code_add_pathz(args)),
            ("code", "add_path", 1 | 2) => Some(self.code_add_pathz(args)),
            ("code", "del_path", 1) => Some(self.code_del_path(args)),
            ("application", "get_env", 1) => Some(self.application_get_env_1()),
            ("application", "get_env", 2) => Some(self.application_get_env_2(args)),
            ("application", "get_env", 3) => Some(self.application_get_env_3(args)),
            ("application", "get_all_env", 1) => Some(self.application_get_all_env_1(args)),
            ("application", "get_key", 2) => Some(self.application_get_key_2(args)),
            ("application", "get_application", 0) => Some(self.application_get_application_0()),
            ("application", "get_application", 1) => {
                Some(self.application_get_application_1(args))
            }
            ("application", "get_all_key", 1) => Some(self.application_get_all_key_1(args)),
            ("application", "get_key", 1) => Some(self.application_get_key_1(args)),
            ("application", "loaded_applications", 0) => {
                Some(self.application_loaded_applications())
            }
            ("application", "which_applications", 0) => {
                Some(self.application_which_applications())
            }
            ("erlang", "make_ref", 0) => Some(self.make_ref_0()),
            ("erlang", "send_after", 3) => Some(self.send_after_3(args)),
            ("erlang", "cancel_timer", 1) => Some(self.cancel_timer_1(args)),
            ("erlang", "spawn", 1) => Some(self.spawn_1(args)),
            ("erlang", "spawn", 2) => Some(self.spawn_2(args)),
            ("erlang", "spawn", 3) => Some(self.spawn_3(args)),
            ("erlang", "spawn_link", 1) => Some(self.spawn_link_1(args)),
            ("erlang", "spawn_link", 2) => Some(self.spawn_link_2(args)),
            ("erlang", "spawn_link", 3) => Some(self.spawn_link_3(args)),
            ("erlang", "spawn_opt", 2) => Some(self.spawn_opt_2(args)),
            ("erlang", "spawn_opt", 3) => Some(self.spawn_opt_3(args)),
            ("erlang", "spawn_opt", 4) => Some(self.spawn_opt_4(args)),
            // `spawn_monitor/1,2,3,4` are `erlang.erl` wrappers (not `bif.tab` BIFs) over
            // `spawn_opt(…, [monitor])`: the whole family is served here so `proc_lib`'s
            // `spawn_mon/3` — hence `gen_server` start-up — resolves. See
            // [`process`](crate::native::process) for the atomic monitor and the node form.
            ("erlang", "spawn_monitor", 1) => Some(self.spawn_monitor_1(args)),
            ("erlang", "spawn_monitor", 2) => Some(self.spawn_monitor_2(args)),
            ("erlang", "spawn_monitor", 3) => Some(self.spawn_monitor_3(args)),
            ("erlang", "spawn_monitor", 4) => Some(self.spawn_monitor_4(args)),
            ("erlang", "monitor", 2) => Some(self.monitor_2(args)),
            ("erlang", "monitor", 3) => Some(self.monitor_3(args)),
            ("erlang", "demonitor", 1) => Some(self.demonitor_1(args)),
            ("erlang", "demonitor", 2) => Some(self.demonitor_2(args)),
            // The **reference-alias** family: `alias/1` (bif.tab:772) mints a reference a
            // process may be sent to; `unalias/1` (bif.tab:773) clears one. Both are
            // runtime-native because the alias table is `beam_process`'s — see
            // [`process`](crate::native::process). `alias/0` is the `erlang.erl` wrapper
            // `alias() -> erlang:alias([])` (`erlang.erl:2132-2133`); `erlang` is a floor
            // namespace, so that wrapper is un-enterable and the floor owns the name, handing
            // the shared implementation its explicit empty option list.
            ("erlang", "alias", 0) => Some(self.alias_0()),
            ("erlang", "alias", 1) => Some(self.alias_1(args)),
            ("erlang", "unalias", 1) => Some(self.unalias_1(args)),
            ("erlang", "exit", 2) => Some(self.exit_2(args)),
            ("erlang", "exit", 3) => Some(self.exit_3(args)),
            ("erlang", "pre_loaded", 0) => Some(self.pre_loaded_0()),
            ("erlang", "module_loaded", 1) => Some(self.module_loaded_1(args)),
            // The `{monitor_nodes, Options}` / bare `monitor_nodes` forms of
            // `erlang:process_flag/2` — the undocumented flag `net_kernel:monitor_nodes/1,2`
            // uses (`bif.c:2202-2227`, `erts_monitor_nodes` at `dist.c:6754-6868`). The
            // `beam_bifs` `ProcessHook` seam cannot carry it (its `process_flag` takes an
            // `Atom` flag, so `process_flag_2` rejects the tuple with `badarg` before the seam
            // is consulted), so the floor serves exactly those two forms and answers `None` for
            // every other flag — an ordinary atom flag still reaches the registry unchanged.
            // See [`process`](crate::native::process).
            ("erlang", "process_flag", 2) => self.process_flag_native(args),
            // `erlang:hibernate/0` is an OTP-30 **instruction** (`emu/ops.tab:896-898`
            // rewrites `call_ext erlang:hibernate/0` to `i_hibernate`), not a `bif.tab` BIF.
            // This runtime's loader performs no such transform, so the call stays a
            // `call_ext`; `erlang` is a floor namespace, so the engine refuses to enter it
            // in-machine and the call reaches this arm. The C's `erts_hibernate`
            // (`beam_common.c:1568-1602`) saves the argument registers, shrinks the heap and
            // clears the process's ACTIVE flag, then returns and execution continues at the
            // next instruction — no result difference for a caller whose next action is the
            // `receive` hibernation precedes — so the floor answers the atom `true` and does
            // nothing else. See [`process`].
            ("erlang", "hibernate", 0) => Some(self.hibernate_0()),
            // The on-demand collection (contract A.7). `erlang:garbage_collect/0` is the
            // `erlang.erl` wrapper over `erts_internal:garbage_collect(major)`
            // (`erlang.erl:3051-3053`), `/1` is the wrapper over
            // `erlang:garbage_collect(Pid, [])` (`erlang.erl:3055-3066`), whose `self()` clause
            // reaches that same `erts_internal` BIF, and `/2` is the option-taking wrapper
            // (`erlang.erl:3073-3157`) — and `erlang` is a floor namespace
            // ([`FLOOR_MODULES`]), so its `.beam` wrapper is un-enterable and the floor owns
            // all three names. The latch, its two serving points and the honest refusals live
            // in [`erts_internal`].
            ("erlang", "garbage_collect", 0) => Some(self.garbage_collect_0()),
            ("erlang", "garbage_collect", 1) => Some(self.garbage_collect_1(args)),
            ("erlang", "garbage_collect", 2) => Some(self.garbage_collect_2(args)),
            // The per-process **group leader** (contract A.9): `erlang:group_leader/2` is the
            // `erlang.erl` wrapper over `erts_internal:group_leader/2,3`, and the leader is
            // runtime state `beam_process` does not model — the store lives on
            // [`Runtime`](crate::Runtime) and every reader shares it. `application_master:init/3`
            // (the boot script's `application:start_boot(kernel, permanent)`) is what needs it.
            // See [`group_leader`].
            ("erlang", "group_leader", 2) => Some(self.group_leader_2(args)),
            // The distribution identity + node-set surface (contract A.5): this node's
            // name, its cookie, the set of connected nodes, and the connector. One arm per
            // name; every answer reads the runtime's own node/link tables, so the
            // per-function bodies -- including the `set_cookie/2` raise shapes the
            // differential corpus pins (`wire_ids/node_identity_cases`) -- live in [`node`]
            // (identity) and [`net`] (the node set + `net_adm`/`net_kernel` connector).
            ("erlang", "is_alive", 0) => Some(self.is_alive_0()),
            ("erlang", "get_cookie", 0) => Some(self.get_cookie_0()),
            ("erlang", "set_cookie", 2) => Some(self.set_cookie_2(args)),
            ("erlang", "nodes", 0) => Some(self.nodes_0()),
            ("erlang", "nodes", 1) => Some(self.nodes_1(args)),
            ("erlang", "nodes", 2) => Some(self.nodes_2(args)),
            ("net_adm", "ping", 1) => Some(self.net_adm_ping_1(args)),
            ("net_kernel", "connect_node", 1) => Some(self.net_kernel_connect_node_1(args)),
            // The Erlang-level distribution supervisor entry that `kernel_sup` starts as its
            // `net_sup` child (`kernel.erl:264`). This runtime's distribution is the native
            // `beam_dist`/`DistNode` (the CLI binds and publishes it itself), so the Erlang
            // socket stack `erl_distribution:init/1` would supervise has nothing to start;
            // the floor answers the atom `ignore` — the same value the oracle's own
            // `erl_distribution:start_link/0` returns when no `-sname`/`-name` argument is
            // present — and `kernel_sup` skips the child. See [`net`].
            ("erl_distribution", "start_link", 0 | 1) => Some(self.erl_distribution_start_link()),
            // Sequential tracing (contract A.8, item 2) — exactly the four BIFs
            // `bif.tab:220-223` declares, served by [`crate::seq_trace`], which also owns the
            // token model and the `system_logger` flag. `system_flag/2` is claimed for the
            // `reset_seq_trace` arm `seq_trace:reset_trace/0` calls and the `system_logger`
            // arm `logger_proxy:init/1` calls (every other flag is that module's typed
            // capability gap); every other arity of those names is not a BIF at all and falls
            // through to the honest `undef`.
            ("erlang", "seq_trace", 2) => Some(self.seq_trace_2(args)),
            ("erlang", "seq_trace_info", 1) => Some(self.seq_trace_info_1(args)),
            ("erlang", "seq_trace_print", 1) => Some(self.seq_trace_print_1(args)),
            ("erlang", "seq_trace_print", 2) => Some(self.seq_trace_print_2(args)),
            ("erlang", "system_flag", 2) => Some(self.system_flag_2(args)),
            // Process tracing (contract A.8, item 1) — the trace-flag state and the four BIFs
            // `bif.tab:217-219` declares. The flag model, `trace_info/2`, `trace_delivered/1`
            // and the function trace patterns live in [`crate::trace`]; `erlang:trace/2,3`
            // and `erlang:trace_pattern/2,3` are the thin wrappers over
            // `erts_internal:trace/3` / `erts_internal:trace_pattern/3`, which the
            // [`erts_internal`] floor serves. Emitting `{trace, …}` events is deliberately out
            // of scope (the installed patterns are the seam a future engine hook reads).
            ("erlang", "trace", 2) => Some(self.erts_trace_2(args)),
            ("erlang", "trace", 3) => Some(self.erts_trace(args)),
            ("erlang", "trace_info", 2) => Some(self.trace_info_2(args)),
            ("erlang", "trace_delivered", 1) => Some(self.trace_delivered_1(args)),
            ("erlang", "trace_pattern", 2) => Some(self.erts_trace_pattern_2(args)),
            ("erlang", "trace_pattern", 3) => Some(self.erts_trace_pattern(args)),
            // The `erts_internal` internal-BIF floor (contract A.7). One arm; the
            // per-function dispatch lives in [`erts_internal`], which answers `None`
            // for anything it does not serve — so `erts_internal:cmp_term/2`, owned
            // by `beam_bifs`, still falls through to the registry.
            ("erts_internal", _, _) => self.erts_internal_call(function, arity, args),
            // The `prim_file` preloaded-module floor (contract A.7): the file-system
            // primitives `file.erl` delegates to. One arm; the per-function dispatch lives
            // in [`prim`], which answers `None` for everything it does not serve — so an
            // unclaimed entry still falls through to the preloaded `.beam` (an honest
            // `erlang:nif_error(undef)`).
            ("prim_file", _, _) => self.prim_file_call(function, arity, args),
            // The `erl_ddll` loaded-driver-list floor: `loaded_drivers/0`, `try_load/3`,
            // `try_unload/2` and `info/2` served from **this** runtime's loaded-driver
            // registry ([`crate::port::driver`]) — the same one an
            // `open_port({spawn_driver, Name}, …)` resolves the name against, so the driver
            // list and the port machinery are one view. One arm; the per-function dispatch
            // lives in [`ddll`], which answers `None` for `monitor/2`/`demonitor/1`/
            // `format_error/1` so `beam_bifs`'s real implementations stay reachable.
            ("erl_ddll", _, _) => self.ddll_call(function, arity, args),
            // The `erl_prim_loader` loader view of the file system (contract A.7) —
            // [`loader`] — a `FLOOR_MODULES` entry so its preloaded `.beam` client half
            // is never entered in-machine. One arm; the per-function dispatch lives in
            // [`loader`], which answers `None` for the archive half of the API.
            ("erl_prim_loader", _, _) => self.loader_call(function, arity, args),
            // The `prim_tty` NIF library's stubs (contract A.7). ERTS serves them from its
            // static NIF table, so `erlang:load_nif("prim_tty", …)` never `dlopen`s a shared
            // object; [`prim_tty`] is the Rust counterpart, and its `NIF_KEYS` is the key
            // list [`floor_shadows`] refuses — so exactly the NIF stubs reach this arm and
            // every unlisted Erlang wrapper (`load/0`, `init/1`, `writer/1`, …) stays
            // enterable in-machine, protecting its `receive`s.
            ("prim_tty", _, _) => self.prim_tty_call(function, arity, args),
            _ => None,
        }
    }

    // -- erlang:halt/0,1,2 ----------------------------------------------------

    /// `erlang:halt/1`: terminate with a status, `abort`, or a crash-dump slogan.
    ///
    /// `halt/1` *is* the `/2` BIF with an **empty** option list (`erts/preloaded/src/erlang.erl`:
    /// `halt(HaltType) -> erlang:halt(HaltType, [])`), so a list argument reaches the
    /// crash-dump branch without any option validation — exactly what the C's `halt_2`
    /// does for its own empty list (`bif.c:4316-4350`).
    fn halt_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        Err(self.halt_with(argument, HaltOptions::default()))
    }

    /// `erlang:halt/2`: `halt/1` plus the `{flush, _}` / `{flush_timeout, _}` options.
    ///
    /// The options are validated **before** the first argument is interpreted (the C's
    /// parse loop runs first, `bif.c:4323-4350`), so `halt(-1, [{flush, maybe}])` reports
    /// the bad option rather than the bad status.
    fn halt_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let options = *args.get(1).ok_or(BeamError::BadArg)?;
        let options = self.halt_options(options)?;
        Err(self.halt_with(argument, options))
    }

    /// The shared body of `halt/1` and `halt/2` (`bif.c:4356-4396`): interpret the first
    /// argument and build the raise that terminates the runtime.
    ///
    /// * a non-negative integer (small **or** bignum) is the exit **status** — the C's
    ///   `code & INT_MAX`;
    /// * the atom `abort` aborts the runtime with `SIGABRT`;
    /// * a list (including `[]`) is a crash-dump **slogan**;
    /// * anything else (a negative integer, a float, an unknown atom, a tuple) is
    ///   `badarg`.
    fn halt_with(&mut self, argument: Eterm, options: HaltOptions) -> BeamError {
        if let Ok(status) = halt_status_of(argument) {
            // A `{flush_timeout, 0}` **with flushing enabled** interrupts the flush as it
            // starts: the C arms the flush timer with a zero timeout and its callback
            // `_exit`s with `ERTS_HALT_FLUSH_TIMEOUT_EXIT_CODE` (`erl_init.c:2805-2808`),
            // so 255 wins over the requested status. With flushing disabled the timeout
            // has no effect at all (`erlang.erl:3449-3450`), so the normal status stands.
            let status = match (options.flush, options.flush_timeout) {
                (true, Some(0)) => HALT_FLUSH_TIMEOUT_EXIT_CODE,
                _ => status,
            };
            return self.record_halt(status);
        }
        if self.atom_name(argument).as_deref() == Some("abort") {
            // `erts_exit(ERTS_ABORT_EXIT, "")` (`bif.c:4373-4378`): the host receives
            // SIGABRT, which the CLI performs with `std::process::abort` (a shell observes
            // exit code 134). The recorded status is 1: a host that intercepts the raise
            // still observes an abnormal termination.
            self.runtime.halt = Some(1);
            return error::abort();
        }
        if beam_term::is_list(argument) || argument == NIL {
            // `erts_exit(ERTS_DUMP_EXIT, "%s\n", halt_msg)` (`bif.c:4379-4394`): the slogan
            // is printed, a crash dump is written and the runtime exits 1. The raise
            // carries the slogan; the CLI writes the dump (see [`crate::crash_dump`]).
            return match halt_slogan(argument) {
                Ok(slogan) => error::dump(&slogan),
                Err(error) => error,
            };
        }
        BeamError::BadArg
    }

    /// Parses the `halt/2` option list (`bif.c:4323-4350`).
    ///
    /// Every element must be a **two-tuple** whose key is `flush` (with the value being
    /// the atom `true` or `false`) or `flush_timeout` (with a non-negative integer value
    /// below `2^32`). A non-list, an improper list, a non-tuple element, an unknown key
    /// and an out-of-range value are all `badarg`, exactly as the C's `bad_options` path.
    ///
    /// The one deliberate divergence: `{flush, maybe}` is `badarg` here, where the oracle
    /// swallows its `am_badopt` extended-info badarg and exits 1 writing a dump with **no
    /// output at all** — an artefact of the C's exit paths, not a documented behaviour, so
    /// the honest `badarg` is reported instead of reproducing it.
    fn halt_options(&self, options: Eterm) -> Result<HaltOptions> {
        let mut parsed = HaltOptions::default();
        for option in terms::list_elements(options)? {
            if terms::tuple_arity(option)? != 2 {
                return Err(BeamError::BadArg);
            }
            let key = self.atom_name(terms::tuple_element(option, 0)?);
            let value = terms::tuple_element(option, 1)?;
            match key.as_deref() {
                Some("flush") => {
                    parsed.flush = match self.atom_name(value).as_deref() {
                        Some("true") => true,
                        Some("false") => false,
                        _ => return Err(BeamError::BadArg),
                    };
                }
                Some("flush_timeout") => {
                    parsed.flush_timeout = Some(halt_timeout_of(value)?);
                }
                _ => return Err(BeamError::BadArg),
            }
        }
        Ok(parsed)
    }

    /// Records `status` on the runtime and returns the matching halt raise.
    fn record_halt(&mut self, status: i32) -> BeamError {
        self.runtime.halt = Some(status);
        error::halt(status)
    }    // -- erlang:display/1 ------------------------------------------------------

    /// `erlang:display/1`: print the term on the process's output (`~p` plus a
    /// newline, as the oracle does) and return `true`.
    fn display(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let text = self.render(term, b"~p~n")?;
        self.write_output(&text)?;
        let true_atom = self.atom("true")?;
        Ok(true_atom)
    }

    // -- erlang:apply/2,3 -----------------------------------------------------

    /// `erlang:apply(Mod, Fun)` / `erlang:apply(Mod, Fun, Args)`.
    ///
    /// Re-enters the host's dispatch with `self` as the context, so the target may be a
    /// native BIF, a `beam_bifs` BIF **or** an external module function loaded on
    /// demand from the code path.
    fn apply_dynamic(&mut self, args: &[Eterm], with_args: bool) -> Result<Eterm> {
        let module = self
            .atom_name(*args.first().ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let function = self
            .atom_name(*args.get(1).ok_or(BeamError::BadArg)?)
            .ok_or(BeamError::BadArg)?;
        let call_args: Vec<Eterm> = if with_args {
            terms::list_elements(*args.get(2).ok_or(BeamError::BadArg)?)?
        } else {
            Vec::new()
        };
        let arity = call_args.len() as u32;
        self.call_bif(&module, &function, arity, &call_args)
    }

    // -- erlang:send_after/3, erlang:cancel_timer/1 ---------------------------

    /// `erlang:send_after(Time, Dest, Msg)`: deliver `Msg` to `Dest` `Time` milliseconds
    /// from now, and return the `#Ref<>` timer reference `erlang:cancel_timer/1` takes.
    ///
    /// The timer is **not** a thread: it is a deadline pushed onto the runtime's own
    /// [`Timers`](crate::timers) heap, which the scheduler loop advances — a pass fires every
    /// deadline that has passed (delivering the message into the target's mailbox through
    /// `beam_process`'s ordinary deliver path, so a blocked receiver is woken exactly as a
    /// `send` wakes it) and, with nothing else to run, sleeps until the earliest one. The
    /// reference comes from [`Runtime::refs_mut`](crate::Runtime::refs_mut) — the runtime's
    /// own id space — and the timer heap registers it, so it is a **bare** runtime-minted
    /// reference that can never resolve as an alias (`Runtime::local_reference`).
    ///
    /// Oracle parity (verified against `erl`, OTP 29):
    ///
    /// * `Time` must be a non-negative integer that fits a monotonic deadline; anything else
    ///   (`-1`, `bad`, a bignum, an absurd delay) raises `badarg` — the C's
    ///   `parse_timeout_pos` failure (`erl_hl_timer.c:2567`);
    /// * `Dest` must be a pid or a registered **name**: `erl_hl_timer.c:1799` answers
    ///   `badarg` for anything else (`send_after(5, foo, x)` with an unregistered atom is
    ///   therefore *accepted* and its message simply finds no process when it fires);
    /// * a delivery to a pid that has since died is dropped, not an error.
    pub(super) fn send_after_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let delay = crate::timers::timer_delay(*args.first().ok_or(BeamError::BadArg)?)?;
        let target = crate::timers::timer_target(*args.get(1).ok_or(BeamError::BadArg)?)?;
        let message = *args.get(2).ok_or(BeamError::BadArg)?;
        let reference = self.mint_timer_ref()?;
        // The message is delivered **later**, by the scheduler loop — long after the process
        // that scheduled the timer may have been collected. Copy it onto the pointer-stable
        // arena now (see `Runtime::try_stabilize_escaped`, which reports a term the copier
        // genuinely cannot copy as the honest `system_limit` instead of storing it). `from`
        // needs no copy (a pid term is a heap-free immediate), and `reference` must **not** be
        // copied: it is the caller's own handle (returned below) *and* the `Timers::refs`
        // lookup key, so a copy would break `erlang:cancel_timer/1`.
        let message = self.runtime.try_stabilize_escaped(message)?;
        let from = self.pid().to_eterm();
        self.runtime
            .timers_mut()
            .schedule(delay, target, from, message, reference);
        Ok(reference)
    }

    /// `erlang:cancel_timer(Ref)`: cancel a pending timer, answering how long was left.
    ///
    /// The oracle's three answers (`cancel_timer_1` → `access_bif_timer`,
    /// `erl_hl_timer.c:2622`, verified against `erl`):
    ///
    /// * a timer reference that is **still pending** → the remaining time in milliseconds,
    ///   and the message is never delivered;
    /// * a reference that already **fired** or was already cancelled → the atom `false`
    ///   (a `#Ref<>` this runtime minted for something else — `make_ref/0`, a monitor —
    ///   answers `false` too);
    /// * anything that is not a reference at all → `badarg`.
    pub(super) fn cancel_timer_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // A timer reference, or a reference this runtime minted for something else
        // (`make_ref/0`/`monitor/2`) — anything else is not a reference and is `badarg`.
        if !self.is_runtime_reference(term) {
            return Err(BeamError::BadArg);
        }
        match self.runtime.timers_mut().cancel(term) {
            Some(remaining) => {
                // Whole milliseconds, truncated — the oracle answers the same unit.
                let millis = i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX);
                Ok(make_small(millis))
            }
            None => self.atom("false"),
        }
    }

    /// Whether `term` is a reference **this runtime minted**: a pending timer's reference or
    /// one a `make_ref/0`/NIF `make_ref`/`monitor/2`/`alias/1`/`seq_trace` path registered.
    ///
    /// This is the argument test `erlang:cancel_timer/1` and `erts_internal:group_leader/3`
    /// share. The C only checks the *shape* (`is_not_internal_ref`), because it can read the
    /// thing's payload; reading one here would need `beam_term::boxed_val`, which is `unsafe`
    /// (and this crate is `#![deny(unsafe_code)]`), so the runtime's term-keyed registries —
    /// the only safe way to recognise a reference — are the test. The comparison is by raw
    /// term word, which is exact for a reference: two distinct references are distinct terms,
    /// and an equal term word denotes the same reference. Delegates to the ONE provenance test,
    /// [`Runtime::is_known_local_reference`].
    pub(super) fn is_runtime_reference(&self, term: Eterm) -> bool {
        self.runtime.is_known_local_reference(term)
    }

    // The delay/destination validation of a timer BIF lives in `crate::timers`
    // (`timer_delay`/`timer_target`) — shared with the `RuntimeHooks` timer family
    // (`erlang:start_timer/*`, `send_after/4`, `cancel_timer/2`) so the validation can
    // never drift between the two entry points.

    /// A fresh `#Ref<>` for a timer, from the runtime's own reference id space.
    ///
    /// The term is built on a pooled BIF heap (see
    /// [`Runtime::take_bif_heap`](crate::Runtime::take_bif_heap)), which never moves or resets
    /// its terms, so it stays valid for the runtime's lifetime. This is the same three-line
    /// mint the process/reference floor uses for `make_ref/0`; it cannot call that module's
    /// private helper, but it draws from the same allocator, so an id is never reused. The term
    /// is registered in the timer heap ([`Timers::schedule`](crate::timers::Timers)), which makes
    /// it a **bare** runtime-minted reference — never a monitor/alias
    /// (`Runtime::local_reference`).
    fn mint_timer_ref(&mut self) -> Result<Eterm> {
        let reference = self.runtime.refs_mut().next();
        let mut heap = self.runtime.take_bif_heap()?;
        let term = reference.to_eterm(&mut heap);
        self.runtime.put_bif_heap(heap);
        term
    }

    // -- erlang:system_info/1 -------------------------------------------------

    /// `erlang:system_info(Key)`: materialize the runtime introspection key (contract
    /// A.8) on the runtime's scratch info heap.
    ///
    /// The key is read as an atom from the shared atom table; the value comes from
    /// [`Runtime::system_info`](crate::Runtime::system_info), the single source of
    /// truth for the supported keys. An unknown key yields
    /// [`BeamError::BadArg`] — the bare `badarg` the oracle raises — so `-eval`'s
    /// `catch`/`try` sees exactly what loaded code would.
    ///
    /// The **tuple** keys the oracle accepts and this runtime answers,
    /// `{allocator, A}` and `{wordsize, internal|external}`, are routed to
    /// [`crate::info::tuple_key_value`] (the `beam_alloc` adoption; anything outside those
    /// two is the oracle's `badarg`).
    /// The report is materialized on the same info heap `Runtime::system_info` uses, so
    /// the returned term has the identical lifetime contract as an atom-key answer.
    fn system_info_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let key_term = *args.first().ok_or(BeamError::BadArg)?;
        if terms::tuple_arity(key_term).is_ok() {
            return self.system_info_tuple(key_term);
        }
        let key = self
            .atom_name(key_term)
            .ok_or(BeamError::BadArg)?;
        self.runtime.system_info(&key)
    }

    /// The `{allocator, A}` / `{wordsize, W}` form of `erlang:system_info/1`.
    ///
    /// Any other 2-tuple tag (or a tuple of another arity) is `badarg` — the oracle
    /// answers no other tuple key this runtime models. Both elements are read as atom
    /// names and handed to [`crate::info::tuple_key_value`], which answers the allocator
    /// report or the word size and raises `badarg` for anything outside them, exactly as
    /// `system_info({allocator, foo_alloc})` and `system_info({wordsize, foo})` do in the
    /// oracle.
    fn system_info_tuple(&mut self, key_term: Eterm) -> Result<Eterm> {
        if terms::tuple_arity(key_term)? != 2 {
            return Err(BeamError::BadArg);
        }
        let tag = self
            .atom_name(terms::tuple_element(key_term, 0)?)
            .ok_or(BeamError::BadArg)?;
        let argument = self
            .atom_name(terms::tuple_element(key_term, 1)?)
            .ok_or(BeamError::BadArg)?;
        let value = crate::info::tuple_key_value(&tag, &argument)?;
        // Materialize on the runtime's own info heap — the same snippet
        // `Runtime::system_info` uses (the tuple path cannot call it, which takes a
        // `&str` key).
        let crate::Runtime {
            info_heap,
            code_server,
            ..
        } = self.runtime;
        info_heap.reset();
        value.to_eterm(info_heap, code_server.atoms())
    }

    // -- os:cmd/1 -------------------------------------------------------------

    /// `os:cmd(Cmd)`: run `Cmd` through `/bin/sh -c` and return its **stdout**.
    ///
    /// `Cmd` is an Erlang string (a char list of bytes) and the result is an Erlang
    /// string (a list of byte small-ints), the trailing newline included. A non-zero
    /// exit status is **not** an error: whatever the command wrote to stdout is returned,
    /// exactly as the oracle does (`os:cmd/1` never consults the status). Only a failure
    /// to spawn `/bin/sh` is raised — the one thing that cannot be recovered from.
    ///
    /// The oracle reaches the same result through the port/spawn **driver** (contract
    /// A.4); Milestone-1 has no port machinery, so the host spawns the process itself.
    fn os_cmd(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let command_term = *args.first().ok_or(BeamError::BadArg)?;
        // `os:cmd/1` takes a *string* (or an iodata binary); unlike `io:put_chars/1` a
        // bare integer is not a character here — the oracle raises `badarg`.
        let is_string =
            terms::list_elements(command_term).is_ok() || terms::binary_bytes(command_term).is_ok();
        if !is_string {
            return Err(BeamError::BadArg);
        }
        let command = self.chars_to_string(command_term)?;
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .output()
            .map_err(|source| {
                error::message(format!("os:cmd/1 cannot run /bin/sh: {source}"))
            })?;
        let bytes: Vec<Eterm> = output
            .stdout
            .iter()
            .map(|byte| make_small(i64::from(*byte)))
            .collect();
        terms::make_list(self, &bytes)
    }

    // -- helpers --------------------------------------------------------------

    /// `strings` as an Erlang list of strings (each a char list).
    pub(super) fn string_list(&mut self, strings: &[String]) -> Result<Eterm> {
        let mut elems = Vec::with_capacity(strings.len());
        for text in strings {
            elems.push(self.charlist(text)?);
        }
        terms::make_list(self, &elems)
    }
    /// The name of the atom `term`, resolved through the runtime's shared atom table.
    pub(super) fn atom_name(&self, term: Eterm) -> Option<String> {
        if !is_atom(term) {
            return None;
        }
        self.runtime
            .code_server
            .atoms()
            .name(atom_val(term))
            .map(str::to_string)
    }

    /// The atom `name`, interned in the runtime's shared atom table.
    pub(super) fn atom(&mut self, name: &str) -> Result<Eterm> {
        let atom = self.runtime.code_server.atoms_mut().intern(name);
        Ok(make_atom(atom))
    }

    /// `text` as an Erlang string (a char list), built on the engine's heap seam.
    pub(super) fn charlist(&mut self, text: &str) -> Result<Eterm> {
        let elems: Vec<Eterm> = text.chars().map(|c| make_small(i64::from(c as u32))).collect();
        terms::make_list(self, &elems)
    }

    /// A term rendered by the registered `io:format` printer, using `format`'s control.
    ///
    /// The scratch heap is reset first, so the text — not any term — is the result.
    fn render(&mut self, term: Eterm, control: &[u8]) -> Result<String> {
        self.runtime.info_heap.reset();
        let runtime = &mut self.runtime;
        format::term_to_string_with(
            &runtime.registry,
            &mut runtime.info_heap,
            runtime.code_server.atoms(),
            runtime.self_pid,
            term,
            control,
        )
    }
    /// Writes `text` to the installed output sink (stdout when none is installed).
    fn write_output(&mut self, text: &str) -> Result<()> {
        use beam_bifs::OutputSink;
        match self.output.as_mut() {
            Some(sink) => sink.write_str(text),
            None => {
                let mut sink = beam_bifs::StdoutSink;
                sink.write_str(text)
            }
        }
    }

    /// Flattens an iodata term (a char list, a nested list, or a binary) to a `String`.
    fn chars_to_string(&self, term: Eterm) -> Result<String> {
        let mut out = String::new();
        self.collect_chars(term, &mut out)?;
        Ok(out)
    }

    /// Appends the characters of `term` to `out` (the [`Self::chars_to_string`] walk).
    fn collect_chars(&self, term: Eterm, out: &mut String) -> Result<()> {
        if is_small(term) {
            let code = signed_value(term);
            let code = u32::try_from(code).map_err(|_| BeamError::BadArg)?;
            let character = char::from_u32(code).ok_or(BeamError::BadArg)?;
            out.push(character);
            return Ok(());
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            out.push_str(&String::from_utf8_lossy(bytes));
            return Ok(());
        }
        if let Ok(elements) = terms::list_elements(term) {
            for element in elements {
                self.collect_chars(element, out)?;
            }
            return Ok(());
        }
        if term == NIL {
            return Ok(());
        }
        Err(BeamError::BadArg)
    }
}

// -- the shared erlang:halt/1,2 helpers (the region above calls them) ---------

/// The exit status a **fired** flush timeout produces
/// (`ERTS_HALT_FLUSH_TIMEOUT_EXIT_CODE`, `erl_init.c:75`).
const HALT_FLUSH_TIMEOUT_EXIT_CODE: i32 = 255;

/// The status mask the C applies to a halt status (`code & INT_MAX`, `bif.c:4357`).
const HALT_STATUS_MASK: u64 = 0x7fff_ffff;

/// The `erlang:halt/2` option set (`bif.c:4318-4321`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HaltOptions {
    /// `{flush, EnableFlushing}` — the C's `flush`, `true` by default.
    flush: bool,
    /// `{flush_timeout, Timeout}` in milliseconds — the C's `halt_flush_timeout`, whose
    /// default `-1` means "infinity" (no timeout).
    flush_timeout: Option<u32>,
}

impl Default for HaltOptions {
    /// The C's initializers: flush on, no flush timeout.
    fn default() -> Self {
        HaltOptions {
            flush: true,
            flush_timeout: None,
        }
    }
}

/// The exit **status** an `erlang:halt/1,2` first argument asks for.
///
/// The C validates it with `term_to_Uint_mask` (`big.c:3544-3565`), which accepts a
/// **non-negative** small integer or bignum and rejects everything else (a negative
/// integer or bignum, a float, an atom, a list, a tuple), and then takes
/// `code & INT_MAX` (`bif.c:4357`) — so `halt(300)` asks for 300, `halt(2**32 + 17)` for
/// 17 and `halt(-1)` is `badarg`. A bignum contributes its **low limb** only: the C picks
/// the first `ErtsDigit` and ignores the rest.
fn halt_status_of(term: Eterm) -> Result<i32> {
    if is_small(term) {
        let value = signed_value(term);
        if value < 0 {
            return Err(BeamError::BadArg);
        }
        return Ok((value as u64 & HALT_STATUS_MASK) as i32);
    }
    match beam_codec::term::big_parts(term) {
        // `(false, digits)` is a non-negative bignum; digits are least significant first,
        // so the first one is exactly the `ErtsDigit` the C reads.
        Ok((false, digits)) => {
            let low = digits.first().copied().unwrap_or(0);
            Ok((low & HALT_STATUS_MASK) as i32)
        }
        _ => Err(BeamError::BadArg),
    }
}

/// The `{flush_timeout, Timeout}` value: a non-negative integer below `2^32`
/// (`term_to_Uint32`, `bif.c:4342`).
///
/// A bignum can never qualify — this runtime's small integers already reach `2^59` — so
/// every non-small term (the atom `infinity`, a negative integer, a float) is the C's
/// `bad_options`.
fn halt_timeout_of(term: Eterm) -> Result<u32> {
    if !is_small(term) {
        return Err(BeamError::BadArg);
    }
    u32::try_from(signed_value(term)).map_err(|_| BeamError::BadArg)
}

/// The `halt("Slogan")` / `halt([], _)` argument as text, with every CR and LF removed.
///
/// The raise that carries the slogan out of the running code has a **single atom** as its
/// reason, so the text may not contain a newline. The C's `erts_unicode_list_to_buf`
/// (`bif.c:4384`) requires a **proper** list of code points — a list is *not* treated as
/// iodata here — and truncates the message at `HALT_MSG_SIZE` (1023) characters, so an
/// improper list or a non-character element is `badarg` and a longer string is truncated
/// exactly as the oracle truncates it.
fn halt_slogan(chars: Eterm) -> Result<String> {
    let mut out = String::new();
    for element in terms::list_elements(chars)?.into_iter().take(HALT_MSG_SIZE) {
        if !is_small(element) {
            return Err(BeamError::BadArg);
        }
        let code = u32::try_from(signed_value(element)).map_err(|_| BeamError::BadArg)?;
        let character = char::from_u32(code).ok_or(BeamError::BadArg)?;
        if character != '\r' && character != '\n' {
            out.push(character);
        }
    }
    Ok(out)
}

/// The C's `HALT_MSG_SIZE` — the slogans' character budget (`bif.c:4380`).
const HALT_MSG_SIZE: usize = 1023;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::test_support::{atom, call, chars, runtime, string_of};

    /// **Drift guard for the cross-module-call seam's exclusion list.**
    ///
    /// The seam (`context::RuntimeContext::resolve_external_call`) must answer `Ok(None)`
    /// for **every** module `try_native` serves — entering such a module's `.beam` would run
    /// the module body *instead of* the host implementation, a wrong result — and for no
    /// other module. Both directions are pinned here against the floor's own dispatch source,
    /// so adding or removing a `try_native` arm without updating [`FLOOR_MODULES`] fails this
    /// test rather than silently mis-routing a call.
    #[test]
    fn every_floor_module_is_a_real_try_native_arm() {
        let source = include_str!("native.rs");
        let match_start = source
            .find("fn try_native(")
            .expect("`try_native` is the floor's dispatch entry point");
        let match_end = match_start
            + source[match_start..]
                .find("\n            _ => None,")
                .expect("`try_native`'s match ends in the `_ => None` fall-through");
        // Every arm's *first* tuple element, taken from the arm lines themselves.
        let arms: Vec<&str> = source[match_start..match_end]
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("(\""))
            .filter_map(|rest| rest.split_once('"').map(|(name, _)| name))
            .collect();
        assert!(
            !arms.is_empty(),
            "the arm scan found no modules — the source layout changed"
        );
        for name in &arms {
            assert!(
                FLOOR_MODULES.contains(name),
                "`try_native` serves `{name}` but `FLOOR_MODULES` omits it: the seam would \
                 enter that module's `.beam` instead of the host implementation"
            );
        }
        for name in FLOOR_MODULES {
            assert!(
                arms.contains(&name),
                "`FLOOR_MODULES` lists `{name}`, which `try_native` does not serve"
            );
        }
    }

    /// **Drift guard for the `io` exception to the floor exclusion.**
    ///
    /// [`floor_shadows`] refuses a whole namespace for ten of the eleven floor modules but
    /// `io` only for the keys the floor really serves (`io:put_chars/1,2`); every other
    /// `io:*` must be entered in-machine, or a scheduled process's io-device `receive` folds
    /// into the typed `blocked()` capability gap. This parses the `("io", "FUNC", ARITY)`
    /// arms out of `try_native`'s own source and pins the exception against them, so adding
    /// or removing a floor `io` arm without updating [`floor_shadows`] fails here rather than
    /// silently mis-routing an `io:*` call.
    #[test]
    fn the_floor_shadows_io_exception_matches_the_floors_io_arms() {
        let source = include_str!("native.rs");
        let match_start = source
            .find("fn try_native(")
            .expect("`try_native` is the floor's dispatch entry point");
        let match_end = match_start
            + source[match_start..]
                .find("\n            _ => None,")
                .expect("`try_native`'s match ends in the `_ => None` fall-through");
        // Every `("io", "FUNC", ARITY)` arm, taken from the arm lines themselves.
        let mut io_keys: Vec<(String, u32)> = source[match_start..match_end]
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("(\"io\", \""))
            .filter_map(|rest| rest.split_once("\", "))
            .map(|(function, tail)| {
                let arity = tail
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|digits| digits.parse::<u32>().ok())
                    .unwrap_or_else(|| panic!("`io` arm `{function}` has no arity literal: {tail:?}"));
                (function.to_string(), arity)
            })
            .collect();
        io_keys.sort();
        assert!(
            !io_keys.is_empty(),
            "the arm scan found no `io` arms — the source layout changed"
        );
        assert_eq!(
            io_keys,
            vec![
                ("format".to_string(), 3),
                ("fwrite".to_string(), 1),
                ("fwrite".to_string(), 2),
                ("fwrite".to_string(), 3),
                ("nl".to_string(), 0),
                ("nl".to_string(), 1),
                ("put_chars".to_string(), 1),
                ("put_chars".to_string(), 2),
                ("write".to_string(), 1),
                ("write".to_string(), 2),
            ],
            "the floor serves the `io` device surface for the `io` namespace — `floor_shadows` \
             must be updated alongside any new/removed floor `io` arm"
        );
        // The floor's `io` keys are refused ...
        for (function, arity) in &io_keys {
            assert!(
                floor_shadows("io", function, *arity),
                "the floor owns `io:{function}/{arity}`, so `floor_shadows` must refuse it"
            );
        }
        // ... and every other `io` key is entered in-machine.
        for (function, arity) in [
            ("getopts", 1u32),
            ("setopts", 2),
            ("request", 2),
            ("scan_erl_form", 4),
            ("format", 2),
        ] {
            assert!(
                !floor_shadows("io", function, arity),
                "`io:{function}/{arity}` is a real `io.erl` body, not a floor key: \
                 `floor_shadows` must let it be entered in-machine"
            );
        }
        // A namespace whose floor arms name single keys stays **key-granular**: every
        // listed key is refused and an unlisted one is entered in-machine. Every other
        // namespace stays refused wholesale.
        for module in FLOOR_MODULES {
            match floor_keys(module) {
                Some(keys) => {
                    for (function, arity) in keys {
                        assert!(
                            floor_shadows(module, function, *arity),
                            "the floor owns `{module}:{function}/{arity}`"
                        );
                    }
                    assert!(
                        !floor_shadows(module, "anything", 9),
                        "`{module}` is key-granular: an unlisted key must be entered in-machine"
                    );
                }
                None => assert!(
                    floor_shadows(module, "anything", 9),
                    "`{module}` is a floor namespace: every key must be refused"
                ),
            }
        }
    }

    /// A tuple built on the runtime's term arena (the `halt/2` argument shape).
    fn tuple(runtime: &mut crate::runtime::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_tuple(&mut context, elements).expect("a tuple")
    }

    /// A proper list built on the runtime's term arena.
    fn list(runtime: &mut crate::runtime::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_list(&mut context, elements).expect("a list")
    }

    /// A `{Key, Value}` option tuple with `Key` interned as an atom.
    fn pair(runtime: &mut crate::runtime::Runtime, key: &str, value: Eterm) -> Eterm {
        let key = atom(runtime, key);
        tuple(runtime, &[key, value])
    }

    #[test]
    fn halt_zero_raises_and_records_the_status() {
        let mut runtime = runtime();
        let error = call(&mut runtime, "erlang", "halt", &[]).expect_err("halt/0 terminates");
        assert_eq!(error::halt_status(&error), Some(0));
        assert_eq!(runtime.halt_status(), Some(0));
        assert_eq!(error::unsupported_reason(&error), None);
    }

    #[test]
    fn halt_one_masks_the_status_like_the_c() {
        let mut runtime = runtime();
        // `code & INT_MAX` (`bif.c:4357`): 300 stays 300 (the OS truncates it to the low
        // byte on exit, so the CLI observes 44) and `2**32 + 17` — a *small* integer for
        // this runtime's 60-bit smalls — keeps only its low 31 bits.
        for (argument, status) in [
            (0i64, 0i32),
            (17, 17),
            (255, 255),
            (300, 300),
            ((1i64 << 32) + 17, 17),
        ] {
            let error = call(&mut runtime, "erlang", "halt", &[make_small(argument)])
                .expect_err("halt/1 terminates");
            assert_eq!(error::halt_status(&error), Some(status), "halt({argument})");
            assert_eq!(runtime.halt_status(), Some(status));
            assert_eq!(error::unsupported_reason(&error), None);
        }
    }

    #[test]
    fn halt_rejects_a_negative_or_non_integer_status() {
        let mut runtime = runtime();
        let x = atom(&mut runtime, "x");
        for argument in [make_small(-1), x] {
            let error =
                call(&mut runtime, "erlang", "halt", &[argument]).expect_err("halt/1 badargs");
            assert!(matches!(error, BeamError::BadArg), "{error}");
            assert_eq!(runtime.halt_status(), None);
        }
    }

    #[test]
    fn halt_abort_is_an_abort_raise() {
        let mut runtime = runtime();
        let abort = atom(&mut runtime, "abort");
        let error = call(&mut runtime, "erlang", "halt", &[abort]).expect_err("halt/1 terminates");
        assert!(error::is_abort(&error), "{error}");
        assert_eq!(error::halt_status(&error), None);
        assert_eq!(error::dump_slogan(&error), None);
        assert_eq!(runtime.halt_status(), Some(1));
    }

    #[test]
    fn halt_with_a_slogan_is_a_dump_raise() {
        let mut runtime = runtime();
        let slogan = chars(&mut runtime, "probe");
        let error = call(&mut runtime, "erlang", "halt", &[slogan]).expect_err("halt/1 terminates");
        assert_eq!(error::dump_slogan(&error), Some("probe"));
        assert!(!error::is_abort(&error));
        assert_eq!(error::unsupported_reason(&error), None);
        // The raise's reason is a single atom, so CR/LF never reach it.
        let slogan = chars(&mut runtime, "a\r\nb");
        let error = call(&mut runtime, "erlang", "halt", &[slogan]).expect_err("halt/1 terminates");
        assert_eq!(error::dump_slogan(&error), Some("ab"));
        // `halt([])` is the empty slogan.
        let error = call(&mut runtime, "erlang", "halt", &[NIL]).expect_err("halt/1 terminates");
        assert_eq!(error::dump_slogan(&error), Some(""));
    }

    #[test]
    fn halt_rejects_every_other_argument_shape() {
        let mut runtime = runtime();
        let key = atom(&mut runtime, "crash_dump");
        let value = chars(&mut runtime, "x");
        let pair = tuple(&mut runtime, &[key, value]);
        let single = tuple(&mut runtime, &[key]);
        for argument in [pair, single] {
            let error =
                call(&mut runtime, "erlang", "halt", &[argument]).expect_err("halt/1 badargs");
            assert!(matches!(error, BeamError::BadArg), "{error}");
        }
    }

    #[test]
    fn halt_two_accepts_the_documented_options() {
        let mut runtime = runtime();
        // `{flush, true}` keeps the requested status …
        let true_atom = atom(&mut runtime, "true");
        let flush_true = pair(&mut runtime, "flush", true_atom);
        let options = list(&mut runtime, &[flush_true]);
        let error = call(&mut runtime, "erlang", "halt", &[make_small(7), options])
            .expect_err("halt/2 terminates");
        assert_eq!(error::halt_status(&error), Some(7));
        // … and `[]` is the `halt/1` form (an empty option list).
        let error = call(&mut runtime, "erlang", "halt", &[make_small(7), NIL])
            .expect_err("halt/2 terminates");
        assert_eq!(error::halt_status(&error), Some(7));
    }

    #[test]
    fn a_zero_flush_timeout_exits_255_unless_flushing_is_disabled() {
        let mut runtime = runtime();
        let timeout_zero = pair(&mut runtime, "flush_timeout", make_small(0));
        // `{flush_timeout, 0}` with flushing on: the C's flush timer fires at once and
        // `_exit`s with `ERTS_HALT_FLUSH_TIMEOUT_EXIT_CODE` = 255.
        let options = list(&mut runtime, &[timeout_zero]);
        let error = call(&mut runtime, "erlang", "halt", &[make_small(3), options])
            .expect_err("halt/2 terminates");
        assert_eq!(error::halt_status(&error), Some(255));
        // With flushing disabled the timeout has no effect: the requested status stands.
        let false_atom = atom(&mut runtime, "false");
        let flush_false = pair(&mut runtime, "flush", false_atom);
        let options = list(&mut runtime, &[flush_false, timeout_zero]);
        let error = call(&mut runtime, "erlang", "halt", &[make_small(3), options])
            .expect_err("halt/2 terminates");
        assert_eq!(error::halt_status(&error), Some(3));
    }

    #[test]
    fn halt_two_rejects_malformed_options() {
        let mut runtime = runtime();
        let maybe = atom(&mut runtime, "maybe");
        let true_atom = atom(&mut runtime, "true");
        let flush_maybe = pair(&mut runtime, "flush", maybe);
        let unknown_key = pair(&mut runtime, "nope", true_atom);
        let negative = pair(&mut runtime, "flush_timeout", make_small(-1));
        let infinity_atom = atom(&mut runtime, "infinity");
        let infinity = pair(&mut runtime, "flush_timeout", infinity_atom);
        // `2**32` is a small integer here but does not fit the C's `Uint32`.
        let too_big = pair(&mut runtime, "flush_timeout", make_small(1i64 << 32));
        let bogus = atom(&mut runtime, "bogus");
        let bad = [
            atom(&mut runtime, "bogus_options"),
            list(&mut runtime, &[bogus]),
            list(&mut runtime, &[unknown_key]),
            list(&mut runtime, &[flush_maybe]),
            list(&mut runtime, &[negative]),
            list(&mut runtime, &[infinity]),
            list(&mut runtime, &[too_big]),
        ];
        for options in bad {
            let error = call(&mut runtime, "erlang", "halt", &[make_small(1), options])
                .expect_err("halt/2 badargs");
            assert!(matches!(error, BeamError::BadArg), "{error}");
            assert_eq!(runtime.halt_status(), None);
        }
    }

    #[test]
    fn display_prints_through_the_output_sink() {
        let mut runtime = runtime();
        let mut sink = beam_bifs::BytesSink::new();
        let displayed = {
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            context
                .call_bif("erlang", "display", 1, &[make_small(42)])
                .expect("display/1")
        };
        assert_eq!(runtime.atom_name_of(displayed), Some("true"));
        assert_eq!(sink.as_str(), "42\n");
    }

    #[test]
    fn apply_of_two_arguments_calls_a_zero_arity_bif() {
        let mut runtime = runtime();
        // `erlang:apply(erlang, self)` — a registered `erlang` BIF reached dynamically.
        let module = atom(&mut runtime, "erlang");
        let function = atom(&mut runtime, "self");
        let self_pid = call(&mut runtime, "erlang", "apply", &[module, function])
            .expect("apply/2 dispatches to the BIF registry");
        assert!(!is_small(self_pid));
        assert!(!is_atom(self_pid));
    }

    #[test]
    fn apply_of_three_arguments_unpacks_the_argument_list() {
        let mut runtime = runtime();
        let module = atom(&mut runtime, "erlang");
        let function = atom(&mut runtime, "+");
        let arguments = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_list(&mut context, &[make_small(2), make_small(3)]).expect("a list")
        };
        let sum = call(&mut runtime, "erlang", "apply", &[module, function, arguments])
            .expect("apply/3");
        assert_eq!(signed_value(sum), 5);
    }

    #[test]
    fn apply_rejects_a_non_atom_target() {
        let mut runtime = runtime();
        let function = atom(&mut runtime, "self");
        assert!(matches!(
            call(&mut runtime, "erlang", "apply", &[make_small(1), function]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn a_non_native_call_falls_through() {
        // The floor must not claim `beam_bifs` surface: `+` stays the registry's.
        let mut runtime = runtime();
        let sum = call(&mut runtime, "erlang", "+", &[make_small(2), make_small(3)]).expect("+");
        assert_eq!(signed_value(sum), 5);
        // A native-shaped call the floor does not own (here: the wrong arity) falls
        // through to the registry and fails there — never silently accepted. The `erlang`
        // namespace has no such name, so it raises the oracle's bare `error:undef`.
        let error = call(&mut runtime, "erlang", "display", &[make_small(1), make_small(2)])
            .expect_err("display/2 is no BIF");
        assert_eq!(error.as_atom(), Some("undef"), "{error}");
    }

    // -- erlang:send_after/3, erlang:cancel_timer/1 ---------------------------

    /// Oracle: `R = erlang:send_after(50, self(), hello), is_reference(R)` ⇒ `true`, and
    /// `erlang:cancel_timer(R)` ⇒ the remaining milliseconds (an integer).
    #[test]
    fn send_after_answers_a_cancellable_reference() {
        let mut runtime = runtime();
        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        let hello = atom(&mut runtime, "hello");
        let reference = call(&mut runtime, "erlang", "send_after", &[make_small(50), me, hello])
            .expect("send_after/3");
        assert!(
            runtime.timers.is_timer_ref(reference),
            "the answer is the timer's own #Ref<>"
        );
        assert_eq!(runtime.timers.pending_count(), 1);

        let remaining =
            call(&mut runtime, "erlang", "cancel_timer", &[reference]).expect("cancel_timer/1");
        assert!(is_small(remaining), "the remaining time is an integer");
        let millis = signed_value(remaining);
        assert!(
            (0..=50).contains(&millis),
            "the remaining time of a 50 ms timer is within its delay: {millis}"
        );
        assert_eq!(runtime.timers.pending_count(), 0, "it will not fire");

        // A reference that already fired or was already cancelled is `false`; a reference
        // this runtime minted for something else (`make_ref/0`) is `false` too.
        let again =
            call(&mut runtime, "erlang", "cancel_timer", &[reference]).expect("cancel_timer/1");
        assert_eq!(runtime.atom_name_of(again).as_deref(), Some("false"));
        let plain = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let answer = call(&mut runtime, "erlang", "cancel_timer", &[plain]).expect("cancel_timer/1");
        assert_eq!(runtime.atom_name_of(answer).as_deref(), Some("false"));
    }

    /// Oracle: `erlang:send_after(10, foo, x)` ⇒ a `#Ref<>` — a registered **name** is a
    /// valid destination (`erl_hl_timer.c:1799` accepts a pid or an atom).
    #[test]
    fn send_after_accepts_a_registered_name() {
        let mut runtime = runtime();
        let name = atom(&mut runtime, "nobody_is_registered");
        let reference =
            call(&mut runtime, "erlang", "send_after", &[make_small(10), name, make_small(1)])
                .expect("an atom destination is accepted");
        assert!(runtime.timers.is_timer_ref(reference));
        assert_eq!(runtime.timers.pending_count(), 1);
    }

    /// Oracle: `erlang:send_after(-1, self(), x)` / `(bad, self(), x)` / `(5, foo, x)` are
    /// `badarg` — a port or any other destination too (`is_not_internal_pid(rcvr) &&
    /// is_not_atom(rcvr)`, `erl_hl_timer.c:1799`).
    #[test]
    fn send_after_validates_the_time_and_the_destination() {
        let mut runtime = runtime();
        let me = call(&mut runtime, "erlang", "self", &[]).expect("self/0");
        let message = make_small(1);
        let bad = atom(&mut runtime, "bad");
        for time in [make_small(-1), bad, NIL] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "send_after", &[time, me, message]),
                    Err(BeamError::BadArg)
                ),
                "a time that is not a non-negative integer is badarg"
            );
        }
        let tuple = {
            let mut sink = beam_bifs::NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_tuple(&mut context, &[make_small(1), make_small(2)]).expect("a tuple")
        };
        for destination in [make_small(7), NIL, tuple] {
            assert!(
                matches!(
                    call(
                        &mut runtime,
                        "erlang",
                        "send_after",
                        &[make_small(5), destination, message]
                    ),
                    Err(BeamError::BadArg)
                ),
                "a destination that is neither a pid nor an atom is badarg"
            );
        }
        assert_eq!(
            runtime.timers.pending_count(),
            0,
            "a refused send_after schedules nothing"
        );
    }

    /// Oracle: `erlang:cancel_timer(foo)` / `(1)` ⇒ `badarg` — only a *reference* is
    /// cancellable, even though a reference that is not a timer answers `false`.
    #[test]
    fn cancel_timer_rejects_a_non_reference() {
        let mut runtime = runtime();
        for term in [make_small(1), NIL] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "cancel_timer", &[term]),
                    Err(BeamError::BadArg)
                ),
                "a term that is not a reference is badarg"
            );
        }
    }

    /// The timer BIFs are no longer capability gaps: `send_after/3` and `cancel_timer/1` are
    /// served by this floor (which runs *before* the registry), and `beam_bifs` registers
    /// **every** declared `bif.tab` key, so the remaining arities resolve through its
    /// `RuntimeHooks` seam — served by [`crate::context::RuntimeHooksBridge`] for the keys it
    /// claims, and the typed `unsupported_bif` gap (exit 3) for the rest. None of them is a
    /// curated gap.
    #[test]
    fn the_timer_bifs_are_no_longer_reported_as_gaps() {
        let mut runtime = runtime();
        for (function, arity) in [
            ("send_after", 3),
            ("cancel_timer", 1),
            ("send_after", 4),
            ("cancel_timer", 2),
            ("start_timer", 3),
            ("start_timer", 4),
        ] {
            assert!(
                runtime.registry.contains("erlang", function, arity),
                "erlang:{function}/{arity} is registry-owned"
            );
            assert!(
                !error::is_known_unimplemented_bif(&runtime.registry, function, arity),
                "erlang:{function}/{arity} must not be classified as a curated gap"
            );
        }
        // `send_after/4` is served by the `RuntimeHooksBridge`: a valid option list
        // schedules and answers the timer reference, which the runtime's own
        // `cancel_timer/1` (this floor) then accepts — so the reference is real.
        let me = runtime.self_pid().to_eterm();
        let msg = runtime.intern("hi");
        let reference = call(
            &mut runtime,
            "erlang",
            "send_after",
            &[make_small(60_000), me, msg, NIL],
        )
        .expect("erlang:send_after/4 with the default option list");
        let remaining = call(&mut runtime, "erlang", "cancel_timer", &[reference])
            .expect("erlang:cancel_timer/1");
        assert!(
            is_small(remaining),
            "cancel_timer/1 answers the remaining milliseconds for a pending timer"
        );
        // An unclaimed key of the same family still resolves as the typed capability gap
        // (exit 3), not the bare `undef` the `erlang`-namespace fallback raises for an
        // unknown name. `read_timer/1` is a key of `beam_bifs`' `RUNTIME_HOOKS_BIFS`
        // roster, so the `RuntimeHooksBridge` — installed for the whole roster — answers
        // it, naming the key in its typed gap rather than the defaulted seam's bare `bif`.
        let error = call(
            &mut runtime,
            "erlang",
            "read_timer",
            &[make_small(0)],
        )
        .expect_err("read_timer/1 has no host hook installed");
        assert_eq!(
            error::unsupported_reason(&error).as_deref(),
            Some("erlang:read_timer/1"),
            "read_timer/1 is the bridge's typed capability gap: {error}"
        );
    }

    // -- os:cmd/1 -------------------------------------------------------------

    #[test]
    fn os_cmd_returns_stdout_as_a_byte_string() {
        let mut runtime = runtime();
        let command = chars(&mut runtime, "echo hi");
        let out = call(&mut runtime, "os", "cmd", &[command]).expect("os:cmd/1");
        // `echo hi` writes "hi\n" — three bytes, the newline included.
        assert_eq!(string_of(out), "hi\n");
    }

    #[test]
    fn os_cmd_does_not_treat_a_nonzero_exit_as_an_error() {
        let mut runtime = runtime();
        let command = chars(&mut runtime, "printf out; exit 3");
        let out = call(&mut runtime, "os", "cmd", &[command])
            .expect("a non-zero exit status is not an error");
        assert_eq!(string_of(out), "out");
    }

    #[test]
    fn os_cmd_rejects_a_non_string_command() {
        let mut runtime = runtime();
        assert!(
            call(&mut runtime, "os", "cmd", &[]).is_err(),
            "arity 1 is required"
        );
        assert!(
            call(&mut runtime, "os", "cmd", &[make_small(7)]).is_err(),
            "an integer is not a command string"
        );
        assert!(
            call(&mut runtime, "os", "cmd", &[NIL]).is_ok(),
            "the empty string is a valid (if useless) command"
        );
    }

}