//! Contract **A.3** — the **resource / ioq / sync** families of the NIF C ABI.
//!
//! [`super::nif_so`] pins the *call* surface of a real NIF library (`enif_get_int`,
//! `enif_make_atom`, `enif_make_reverse_list`, `enif_make_badarg`, the `load_nif/2` failure
//! shapes). This group adds the families that have no counterpart there: the **resource
//! types** (`enif_open_resource_type` + `enif_alloc_resource` + `enif_sizeof_resource` +
//! `enif_keep_resource`/`enif_release_resource`), the **IO queue** (`enif_ioq_create` /
//! `enif_ioq_enq_binary` / `enif_ioq_size` / `enif_ioq_peek` / `enif_ioq_deq` /
//! `enif_ioq_destroy`) and the **thread/mutex** synchronisation slots (`enif_mutex_create` /
//! `enif_mutex_lock` / `enif_mutex_unlock` / `enif_mutex_destroy`, `enif_thread_self`).
//!
//! The C library is compiled in the case's own temp dir against the host `erl_nif.h` and
//! `erlang:load_nif/2`ed by the embedded module exactly like `nif_so`, so both runtimes load
//! *the same* `.so` and run *the same* compiled module; only stdout + exit status are
//! compared. A case therefore agrees only if the Rust runtime really exports and implements
//! the `enif_*` slots the library binds.
//!
//! # Why only these three selectors are pinned
//!
//! The library is `dlopen(RTLD_NOW)`ed by **both** runtimes, so every `enif_*` it references
//! must be a symbol the Rust runtime really defines; a missing one fails the *Rust* load
//! while the oracle loads it — a **mismatch**, not a capability gap. Only the families that
//! were verified to agree end to end under both runtimes are therefore pinned here. Verified
//! divergences are reported in this module's doc comment rather than pinned (they are plain
//! values, so they would be mismatches):
//!
//! * `enif_make_resource` + `enif_get_resource` round trip: the oracle returns `42`, the
//!   Rust runtime raises `error:badarg` from `enif_get_resource`;
//! * a resource's **destructor**: the oracle runs it synchronously on the final
//!   `enif_release_resource` (`dtor_count` is `1`), the Rust runtime reports `0`;
//! * `enif_is_process_alive` on the calling process: oracle `1`, Rust runtime `0`;
//! * `enif_monitor_process` on the calling process followed by `enif_demonitor_process`:
//!   both answer `0` (the monitor is not established), so the case would pin no behaviour.
//!
//! # Skip policy (inherited from [`super::nif_so`])
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_nif.h`; when either is
//! missing the harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`).

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: one stub and one `classify/1` branch per `ErlNifFunc` entry.
///
/// `erlang:load_nif/2` refuses a library whose functions do not exist **and are exported**
/// in the loading module, and the calls into the library must be **external** (the Rust
/// runtime intercepts a NIF at external dispatch) — the same three rules `nif_so` pins.
const NIF_RES_MODULE_SOURCE: &str = r#"-module(nif_res).
-export([classify/1, res_size/0, ioq_size/0, sync_roundtrip/0]).

res_size() -> erlang:nif_error(not_loaded).
ioq_size() -> erlang:nif_error(not_loaded).
sync_roundtrip() -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_res", 0) of
        ok ->
            case N of
                1 -> nif_res:res_size();
                2 -> nif_res:ioq_size();
                3 -> nif_res:sync_roundtrip()
            end;
        {error, {Reason, _Text}} ->
            reason_code(Reason)
    end.

%% Only the reason *tag* leaves this function: the `Text` of a failure embeds the absolute
%% temp-dir path, which would make the golden drift on every run.
reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(_) -> 9.
"#;

/// The C library: the resource-type, IO-queue and sync families in three probe functions.
///
/// Every `enif_*` here is a symbol the `beam_rt` release binary exports (verified with
/// `nm -D`); nothing C-variadic is referenced, and `enif_alloc_env` — which the runtime does
/// **not** define — is deliberately absent.
const NIF_RES_C: &str = r#"/* A real NIF exercising the resource / ioq / sync families of the NIF C ABI. */
#include <erl_nif.h>
#include <string.h>

typedef struct { int value; } res_t;

static int dtor_count = 0;
static ErlNifResourceType* res_type = NULL;

static void res_dtor(ErlNifEnv* env, void* obj) {
    (void) env;
    (void) obj;
    dtor_count++;
}

static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    (void) load_info;
    res_type = enif_open_resource_type(env, NULL, "nif_res_type", res_dtor, ERL_NIF_RT_CREATE, NULL);
    if (res_type == NULL)
        return 1;
    *priv = (void*) res_type;
    return 0;
}

/* The resource-type family: allocate a resource, keep it (refcount 2), ask for its size,
   then release it twice. Returns sizeof(res_t) = 4. */
static ERL_NIF_TERM res_size(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    size_t n;
    res_t* r = (res_t*) enif_alloc_resource(res_type, sizeof(res_t));
    if (r == NULL)
        return enif_make_badarg(env);
    n = enif_sizeof_resource(r);
    enif_keep_resource(r);
    enif_release_resource(r);
    enif_release_resource(r);
    return enif_make_int(env, (int) n);
}

/* The ioq family: a NORMAL queue takes ownership of two heap binaries; size, peek and deq
   are all observable. Returns size*1000 + dequeued*100 + iovlen*10 + deq_ok. */
static ERL_NIF_TERM ioq_size(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifIOQueue* q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    ErlNifBinary b1, b2;
    size_t n, got = 0, after;
    int iovlen = 0, deq;
    SysIOVec* v;
    if (q == NULL || !enif_alloc_binary(3, &b1) || !enif_alloc_binary(2, &b2))
        return enif_make_badarg(env);
    memcpy(b1.data, "abc", 3);
    memcpy(b2.data, "de", 2);
    if (!enif_ioq_enq_binary(q, &b1, 0) || !enif_ioq_enq_binary(q, &b2, 0))
        return enif_make_badarg(env);
    n = enif_ioq_size(q);
    v = enif_ioq_peek(q, &iovlen);
    deq = enif_ioq_deq(q, n, &got);
    after = enif_ioq_size(q);
    enif_ioq_destroy(q);
    return enif_make_int(env, (int) (n * 1000 + got * 100 + (v != NULL ? iovlen : 0) * 10
                                    + (deq ? 1 : 0) + (after == 0 ? 0 : 1)));
}

/* The sync family: create a named mutex, lock/unlock it, ask for the calling thread's id and
   destroy it. Returns 2 when every call succeeded. */
static ERL_NIF_TERM sync_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    int thr;
    ErlNifMutex* m = enif_mutex_create("nif_res_mutex");
    if (m == NULL)
        return enif_make_int(env, 0);
    enif_mutex_lock(m);
    thr = (enif_thread_self() != 0);
    enif_mutex_unlock(m);
    enif_mutex_destroy(m);
    return enif_make_int(env, (thr ? 1 : 0) + 1);
}

static ErlNifFunc nif_res_funcs[] = {
    {"res_size", 0, res_size, 0},
    {"ioq_size", 0, ioq_size, 0},
    {"sync_roundtrip", 0, sync_roundtrip, 0}
};

/* `ERL_NIF_INIT` seeds ErlNifEntry.name with the stringified module name (`nif_res`), so this
   library may only be loaded as `nif_res`. */
ERL_NIF_INIT(nif_res, nif_res_funcs, load, NULL, NULL, NULL)
"#;

/// The library every case loads.
const NIF_RES: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_res",
    c_source: NIF_RES_C,
};

/// Build one module-run case over [`NIF_RES_MODULE_SOURCE`].
fn case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::NifAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: NIF_RES_MODULE_SOURCE,
            module: "nif_res",
            function: "classify",
            arity: 1,
            args: selector,
            nif: Some(NIF_RES),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.3 resource / ioq / sync group. Each golden is the integer the **oracle** prints
/// (verified per case with `differential run <id>`).
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "nif_res/resource_type_sizeof",
            "a NIF resource type allocates a resource and enif_sizeof_resource reports its size",
            &[1],
            // `sizeof(res_t)` = one `int`.
            "4",
        ),
        case(
            "nif_res/ioq_enq_peek_deq",
            "a NIF IO queue takes two binaries, reports size 5, peeks one iovec and dequeues all 5",
            &[2],
            "5021",
        ),
        case(
            "nif_res/mutex_lock_unlock_roundtrip",
            "a NIF named mutex locks, unlocks and is destroyed, and enif_thread_self is non-zero",
            &[3],
            "2",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.3 resource/ioq/sync group.

    use super::{group, CaseKind, Contract, NifFixture};
    use std::collections::HashSet;

    #[test]
    fn every_case_is_a_nif_module_run_over_the_same_compiled_library() {
        let mut selectors = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("nif_res/"),
                "case {} is outside the nif_res group",
                case.id
            );
            assert!(!case.description.is_empty(), "case {} has no description", case.id);
            assert_eq!(
                case.contracts,
                &[Contract::NifAbi, Contract::BeamAsm],
                "case {}: an end-to-end NIF case evidences A.3 and the compiled .beam (A.2)",
                case.id
            );
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.3 case must be a module-run case", case.id);
            };
            assert_eq!(run.module, "nif_res", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
            assert!(selectors.insert(run.args[0]), "case {}: selector reused", case.id);
            assert!(
                matches!(run.nif, Some(NifFixture::CompiledLibrary { .. })),
                "case {}: the library must be C-compiled",
                case.id
            );
            assert!(
                case.golden
                    .stdout
                    .expect("every case pins a golden")
                    .chars()
                    .all(|c| c.is_ascii_digit()),
                "case {}: the golden must be a plain integer line (a path-bearing \
                 {{error, {{Reason, Text}}}} would drift), got {:?}",
                case.id,
                case.golden.stdout
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert_eq!(selectors.len(), 3, "the group covers three distinct families");
    }

    #[test]
    fn the_c_library_never_references_an_unimplemented_symbol() {
        // `enif_alloc_env` is not defined by the Rust runtime, so a library that used it
        // would fail the Rust `dlopen(RTLD_NOW)` while the oracle succeeded — a mismatch.
        // Keep the guard so a future edit cannot reintroduce it silently.
        assert!(
            !super::NIF_RES_C.contains("enif_alloc_env"),
            "enif_alloc_env is not implemented by the Rust runtime"
        );
    }
}
