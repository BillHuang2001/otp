/*
 * A driver shared object that RECORDS its `finish` callback (workspace contract A.4).
 *
 * Used by `tests/driver_loader.rs`. It is compiled at test time with the system `cc` against the
 * system `erl_driver.h` and loaded through `beam_abi::driver::loader::load`, then dropped: the
 * loader's `DriverLibrary::drop` must run this entry's `finish` callback *before* `dlclose`, which
 * is what the C's `do_unload_driver_entry` does (`erts/emulator/beam/erl_bif_ddll.c:1559-1564`).
 *
 * `driver_name` is `"t"`, so the only name this library may be loaded as is `t`. `control/…` is a
 * trivial echo (`command + 1`) so the test can call a callback wrapper on the loaded entry.
 *
 * `finish` appends a line to the file named by the environment variable
 * `BEAM_ABI_DRIVER_LOADER_FINISH` (the test points it at a fresh scratch file). The test process
 * and the driver share one address space (the `.so` is `dlopen`ed into the test binary), so
 * `getenv` sees the test's value; a missing/empty variable makes the callback a silent no-op.
 *
 * Portability note: this fixture references NO `driver_*` / `erl_drv_*` symbol, so
 * `dlopen(RTLD_NOW)` succeeds without the runtime's exports (the exports are exercised by the other
 * driver fixtures, e.g. `test_driver.c`).
 */
#include <erl_driver.h>
#include <stdio.h>
#include <stdlib.h>

static void t_finish(void) {
    const char *path = getenv("BEAM_ABI_DRIVER_LOADER_FINISH");
    FILE *f;
    if (path == NULL || path[0] == '\0')
        return;
    f = fopen(path, "a");
    if (f != NULL) {
        fputs("finish\n", f);
        fclose(f);
    }
}

static ErlDrvSSizeT t_control(ErlDrvData drv_data, unsigned int command,
                              char *buf, ErlDrvSizeT len,
                              char **rbuf, ErlDrvSizeT rlen) {
    (void) drv_data; (void) buf; (void) len; (void) rbuf; (void) rlen;
    return (ErlDrvSSizeT) (command + 1);
}

static ErlDrvEntry t_driver_entry = {
    .init = NULL,
    .start = NULL,
    .stop = NULL,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "t",
    .finish = t_finish,
    .handle = NULL,
    .control = t_control,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = 0,
    .handle2 = NULL,
    .process_exit = NULL,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(t) {
    return &t_driver_entry;
}
