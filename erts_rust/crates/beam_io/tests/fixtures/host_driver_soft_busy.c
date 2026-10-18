/*
 * The `ERL_DRV_FLAG_SOFT_BUSY` build of the `beam_io` C driver fixture (workspace contract A.4: the
 * frozen driver C ABI).
 *
 * `host_driver.c` builds an entry whose `driver_flags` is `0` unless
 * `HOST_DRIVER_DRIVER_FLAGS_SLOT` is defined first (see the "## The variant builds" note in that
 * file). Zero flags is the shape `erlang:port_command/3`'s `[force]` **refuses** with
 * `error:notsup` (`erts/emulator/beam/erl_bif_port.c:195-198`); the flag below is the one that
 * makes a forced command skip the port's busy state instead
 * (`ERL_DRV_FLAG_SOFT_BUSY`, `erl_driver.h`).
 *
 * So this build and `host_driver.c` are the two sides of the same C branch: with the flag, a
 * `[force]` command on a port the driver marked busy (the `busy` start token, or control verb 25)
 * is written; without it, the same command raises `notsup` and writes nothing.
 *
 * `#include "host_driver.c"` resolves relative to this file's own directory (both fixtures live in
 * `tests/fixtures/`), so the variant compiles to the same driver with just the flag word added; the
 * `driver_name` stays `"host_driver"`, which is why it is loaded under that name too.
 */
#define HOST_DRIVER_DRIVER_FLAGS_SLOT ERL_DRV_FLAG_SOFT_BUSY
#include "host_driver.c"
