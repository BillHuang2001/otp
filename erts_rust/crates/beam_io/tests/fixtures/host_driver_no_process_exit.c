/*
 * The `process_exit`-less build of the `beam_io` C driver fixture (workspace contract A.4).
 *
 * `host_driver.c` builds an `ErlDrvEntry` whose `process_exit` slot is `host_process_exit` unless
 * `HOST_DRIVER_PROCESS_EXIT_SLOT` is defined first (see the "The two builds" note in that file).
 * Defining it to `NULL` here and then including the fixture produces the entry a driver with no
 * `process_exit` callback has — the one that makes the host's port-exit adapter
 * (`DriverPort::port_exit`, `src/abi/port.rs`) take its **`emergency_close` fallback** instead: a
 * driver whose only exit hook is `emergency_close` must still observe the port-exit notification.
 * The `emergency_close` slot keeps the fixture's own `host_emergency_close`, so the fallback is
 * observable as an `"emergency_close"` log line and an `"E"` `driver_output` marker.
 *
 * `#include "host_driver.c"` resolves relative to this file's own directory (both fixtures live in
 * `tests/fixtures/`), so the variant compiles to the same code with just the one slot changed; the
 * `driver_name` stays `"host_driver"`, which is why it is loaded under that name too.
 */
#define HOST_DRIVER_PROCESS_EXIT_SLOT NULL
#include "host_driver.c"
