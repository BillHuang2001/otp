/*
 * The `ready_async`-less build of the `beam_io` C driver fixture (workspace contract A.4).
 *
 * `host_driver.c` builds an `ErlDrvEntry` whose `ready_async` slot is `host_ready_async` unless
 * `HOST_DRIVER_READY_ASYNC_SLOT` is defined first (see the "The two builds" note in that file).
 * Defining it to `NULL` here and then including the fixture produces the variant entry an entry
 * *without* a `ready_async` callback has — the one that makes the host take its `async_free` arm
 * instead (the C's `need_free` path, `io.c:5399-5406`): with no `ready_async` to hand a finished
 * `driver_async` job to, the host must release the job's `async_data` through the driver's
 * `async_free` itself.
 *
 * `#include "host_driver.c"` resolves relative to this file's own directory (both fixtures live in
 * `tests/fixtures/`), so the variant compiles to the same code with just the one slot changed; the
 * `driver_name` stays `"host_driver"`, which is why it is loaded under that name too.
 */
#define HOST_DRIVER_READY_ASYNC_SLOT NULL
#include "host_driver.c"
