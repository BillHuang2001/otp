/*
 * The **marker** build of the `beam_io` `flush` fixture (`src/abi/fixtures/flush_driver.c`,
 * workspace contract A.4).
 *
 * `flush_driver.c` makes no host calls so that it loads in any test binary (see "Why the base fixture
 * references no `driver_*` function" there). Defining `FLUSH_DRIVER_EMIT_MARKER` here turns on the two
 * `driver_output` calls the data-path half needs: `flush` hands a one-byte `"F"` marker up to the
 * port, and `output` echoes what the port's close-time drain hands it.
 *
 * A driver `.so` leaves `driver_*` undefined and resolves them out of the loading process's **dynamic**
 * symbol table, so this build only `dlopen`s where the host symbols are exported (`build.rs`'s
 * `-Wl,--export-dynamic` covers the integration test targets, not the library's own unit-test binary).
 * The suite therefore probes with `dlsym(RTLD_DEFAULT, …)` first and prints a loud `SKIP:` banner when
 * they are absent, rather than reporting a wiring bug that is not one.
 *
 * The `driver_name` stays `"flush_driver"`, so it is loaded under that name too.
 */
#define FLUSH_DRIVER_EMIT_MARKER
#include "flush_driver.c"
