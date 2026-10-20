/*
 * The **`flush`-less** build of the `beam_io` `flush` fixture (`src/abi/fixtures/flush_driver.c`,
 * workspace contract A.4).
 *
 * `flush_driver.c` builds an `ErlDrvEntry` whose `flush` slot is `flush_cb` unless
 * `FLUSH_DRIVER_FLUSH_SLOT` is defined first. Defining it to `NULL` here produces the entry a
 * driver with nothing to discard has — the C's `if (p->drv_ptr->flush != NULL)` no-op
 * (`erts/emulator/beam/io.c:3663,3666`) — so a host can pin that the host's `DriverPort::flush`
 * forwarding swallows the loader's `absent("flush")` instead of turning it into an error: the port
 * still closes, and the driver's `stop` still runs.
 *
 * `#include "flush_driver.c"` resolves relative to this file's own directory (both fixtures live in
 * `src/abi/fixtures/`), so the variant compiles to the same driver with just the one slot changed;
 * the `driver_name` stays `"flush_driver"`, which is why it is loaded under that name too.
 */
#define FLUSH_DRIVER_FLUSH_SLOT NULL
#include "flush_driver.c"
