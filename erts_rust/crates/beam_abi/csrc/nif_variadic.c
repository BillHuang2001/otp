/*
 * C-variadic shims for the seven `ErlNifApi` slots that stable Rust cannot define.
 *
 * A `...` in a Rust `fn` *definition* is a nightly-only feature (rust-lang/rust#44930), so the seven
 * C-variadic `ErlNifApi` slots
 *
 *     enif_make_tuple, enif_make_list, enif_set_option,
 *     enif_fprintf, enif_snprintf, enif_vfprintf, enif_vsnprintf
 *
 * are defined here with the exact signatures of `erts/emulator/beam/erl_nif_api_funcs.h`, and each
 * one forwards immediately to a `#[no_mangle]` Rust helper in `../src/nif/enif/variadic.rs`, which
 * owns every bit of the logic. This file only does what Rust cannot: touch `va_list`.
 *
 * It is compiled by `build.rs` with the *bare* C compiler (see `build.rs`) — deliberately **no OTP
 * include path** — so the handful of ABI types it needs are declared locally below. The frozen
 * `beam_abi` target is 64-bit LP64 (Linux x86-64 / AArch64), where `ERL_NIF_TERM` is `unsigned long`
 * and `ErlNifEnv` is an opaque struct behind a pointer.
 *
 * # How the varargs reach Rust
 *
 * Rust cannot read a `va_list`, so it *drives* the extraction: `beam_abi_enif_fmt_analyze` parses the
 * ERTS format string and reports, for every conversion, which C vararg type it consumes (the
 * `BEAM_ABI_ARG_*` codes below). `beam_abi_format` then pulls exactly those varargs into a flat
 * `uint64_t` slot array (`double` bits copied verbatim) and hands the array to Rust, which renders
 * the whole string. The format grammar — and therefore the kind list — is Rust's, so the parser and
 * the extractor can never disagree.
 *
 * # `long` vs `long long`
 *
 * A `BEAM_ABI_ARG_I64` slot is pulled with `va_arg(ap, long long)`. On every LP64 ABI `long`,
 * `long long` and `unsigned long` (i.e. `ERL_NIF_TERM`/`Eterm`) are one and the same 8-byte
 * INTEGER-class argument, so reading them through `long long` is ABI-exact on the supported target.
 * Likewise every sub-`int` argument is promoted to `int` by the C call ABI, which is why a single
 * `BEAM_ABI_ARG_I32` code covers `char`, `short` and `int`.
 */

#include <errno.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* --- the minimal ABI types (no OTP headers here) ---------------------------------------------- */

/* `ERL_NIF_TERM`/`Eterm` — one machine word, `unsigned long` on LP64. */
typedef unsigned long beam_abi_term;
/* Opaque `ErlNifEnv` — only ever used behind a pointer. */
struct beam_abi_env;
/* `ErlNifOption` — a 4-byte C enum (`ERL_NIF_OPT_*`). */
typedef int beam_abi_option;

/* --- the Rust helpers (`../src/nif/enif/variadic.rs`) ----------------------------------------- */

extern beam_abi_term beam_abi_enif_make_tuple_from_terms(struct beam_abi_env *env, unsigned cnt,
                                                         const beam_abi_term *terms);
extern beam_abi_term beam_abi_enif_make_list_from_terms(struct beam_abi_env *env, unsigned cnt,
                                                        const beam_abi_term *terms);
extern int beam_abi_enif_set_option(struct beam_abi_env *env, beam_abi_option opt, void *callback);
extern ptrdiff_t beam_abi_enif_fmt_analyze(const char *format, unsigned char *kinds, size_t cap);
extern ptrdiff_t beam_abi_enif_fmt_snprintf(char *out, size_t out_len, const char *format,
                                            const uint64_t *slots, size_t nargs);
extern ptrdiff_t beam_abi_enif_fmt_fprintf(FILE *filep, const char *format, const uint64_t *slots,
                                           size_t nargs);

/* --- the vararg-kind codes (mirrored by `variadic.rs`) ---------------------------------------- */

#define BEAM_ABI_ARG_NONE 0 /* consumes no vararg                                        */
#define BEAM_ABI_ARG_I32 1  /* `int` (every sub-int type is promoted to it)              */
#define BEAM_ABI_ARG_I64 2  /* `long`/`long long`/`unsigned long`/term/fixnum           */
#define BEAM_ABI_ARG_F64 3  /* `double`                                                  */
#define BEAM_ABI_ARG_PTR 4  /* any object pointer (`char*`, `void*`, ...)                */

/* Terms pulled on the stack up to this count; a larger `cnt` uses the heap. */
#define BEAM_ABI_MAX_STACK_TERMS 32

/* --- shared helpers ---------------------------------------------------------------------------- */

/*
 * Pull `cnt` terms out of `ap` and build a tuple or a list from them.
 *
 * The count is caller-supplied (`enif_make_tuple(env, 3, a, b, c)`), so a bogus count reads whatever
 * the caller left in the variadic registers — exactly as in the C runtime, where the count is
 * trusted. A heap fallback keeps a large count off the C stack; if even that allocation fails the
 * NULL array is handed to Rust, which reports its own `badarg` instead of a fabricated term.
 */
static beam_abi_term beam_abi_gather(struct beam_abi_env *env, unsigned cnt, va_list ap,
                                     int want_tuple) {
    beam_abi_term stack_terms[BEAM_ABI_MAX_STACK_TERMS];
    beam_abi_term *terms = stack_terms;
    const beam_abi_term *arg = NULL;
    unsigned i;

    if (cnt > BEAM_ABI_MAX_STACK_TERMS) {
        terms = (beam_abi_term *)malloc(sizeof(beam_abi_term) * (size_t)cnt);
        if (!terms) {
            return want_tuple ? beam_abi_enif_make_tuple_from_terms(env, cnt, NULL)
                              : beam_abi_enif_make_list_from_terms(env, cnt, NULL);
        }
    }
    if (cnt) {
        for (i = 0; i < cnt; i++) {
            terms[i] = va_arg(ap, beam_abi_term);
        }
        arg = terms;
    }
    {
        beam_abi_term result = want_tuple ? beam_abi_enif_make_tuple_from_terms(env, cnt, arg)
                                          : beam_abi_enif_make_list_from_terms(env, cnt, arg);
        if (terms != stack_terms) {
            free(terms);
        }
        return result;
    }
}

/*
 * The shared driver for all four `*printf` slots: ask Rust which varargs the format consumes, pull
 * exactly those, and let Rust render the result either into `out` (a `size`-bounded buffer, with the
 * oracle's truncation semantics) or onto `filep`.
 *
 * `out` is only consulted when `filep` is NULL, and vice versa.
 */
static int beam_abi_format(FILE *filep, char *out, size_t out_len, const char *format, va_list ap) {
    unsigned char *kinds = NULL;
    uint64_t *slots = NULL;
    ptrdiff_t n;
    ptrdiff_t i;
    ptrdiff_t result;

    if (format == NULL) {
        return -EINVAL;
    }

    /* Pass 1: how many varargs does the format consume (and are they all supported)? */
    n = beam_abi_enif_fmt_analyze(format, NULL, 0);
    if (n < 0) {
        return (int)n;
    }

    if (n > 0) {
        kinds = (unsigned char *)malloc((size_t)n);
        slots = (uint64_t *)malloc(sizeof(uint64_t) * (size_t)n);
        if (!kinds || !slots) {
            free(kinds);
            free(slots);
            return -ENOMEM;
        }
        /* Pass 2: fill in the kinds. The parser is deterministic, so this cannot disagree. */
        if (beam_abi_enif_fmt_analyze(format, kinds, (size_t)n) != n) {
            free(kinds);
            free(slots);
            return -EINVAL;
        }
        for (i = 0; i < n; i++) {
            switch (kinds[i]) {
            case BEAM_ABI_ARG_I32:
                slots[i] = (uint64_t)(long long)va_arg(ap, int);
                break;
            case BEAM_ABI_ARG_I64:
                slots[i] = (uint64_t)va_arg(ap, long long);
                break;
            case BEAM_ABI_ARG_F64: {
                double d = va_arg(ap, double);
                uint64_t bits;
                memcpy(&bits, &d, sizeof(bits));
                slots[i] = bits;
                break;
            }
            case BEAM_ABI_ARG_PTR:
                slots[i] = (uint64_t)(uintptr_t)va_arg(ap, void *);
                break;
            default:
                /*
                 * An unknown kind means Rust and this file disagree about the argument list; the
                 * remaining slots would be misaligned. Refuse instead of fabricating a value.
                 */
                free(kinds);
                free(slots);
                return -EINVAL;
            }
        }
    }

    if (filep) {
        result = beam_abi_enif_fmt_fprintf(filep, format, slots, (size_t)n);
    } else {
        result = beam_abi_enif_fmt_snprintf(out, out_len, format, slots, (size_t)n);
    }

    free(kinds);
    free(slots);
    return (int)result;
}

/* --- the seven C-variadic `enif_*` definitions -------------------------------------------------- */

beam_abi_term enif_make_tuple(struct beam_abi_env *env, unsigned cnt, ...) {
    va_list ap;
    beam_abi_term result;

    va_start(ap, cnt);
    result = beam_abi_gather(env, cnt, ap, 1);
    va_end(ap);
    return result;
}

beam_abi_term enif_make_list(struct beam_abi_env *env, unsigned cnt, ...) {
    va_list ap;
    beam_abi_term result;

    va_start(ap, cnt);
    result = beam_abi_gather(env, cnt, ap, 0);
    va_end(ap);
    return result;
}

int enif_set_option(struct beam_abi_env *env, beam_abi_option opt, ...) {
    va_list ap;
    void *callback = NULL;

    switch (opt) {
    /* Both take one pointer argument; every other option takes none. */
    case 2 /* ERL_NIF_OPT_ON_HALT */:
    case 3 /* ERL_NIF_OPT_ON_UNLOAD_THREAD */:
        va_start(ap, opt);
        callback = va_arg(ap, void *);
        va_end(ap);
        break;
    default:
        break;
    }
    return beam_abi_enif_set_option(env, opt, callback);
}

int enif_fprintf(FILE *filep, const char *format, ...) {
    va_list ap;
    int result;

    va_start(ap, format);
    result = beam_abi_format(filep, NULL, 0, format, ap);
    va_end(ap);
    return result;
}

int enif_vfprintf(FILE *filep, const char *format, va_list ap) {
    return beam_abi_format(filep, NULL, 0, format, ap);
}

int enif_snprintf(char *buffer, size_t size, const char *format, ...) {
    va_list ap;
    int result;

    va_start(ap, format);
    result = beam_abi_format(NULL, buffer, size, format, ap);
    va_end(ap);
    return result;
}

int enif_vsnprintf(char *buffer, size_t size, const char *format, va_list ap) {
    return beam_abi_format(NULL, buffer, size, format, ap);
}
