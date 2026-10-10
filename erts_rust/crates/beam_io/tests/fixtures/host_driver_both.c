/*
 * The "all three write-side callbacks" build of the `beam_io` C driver fixture (workspace contract
 * A.4: the frozen driver C ABI).
 *
 * `host_driver.c` builds an entry whose `outputv` slot is `NULL` unless `HOST_DRIVER_OUTPUTV_SLOT`
 * is defined first (see the "## The variant builds" note in that file); its `output` is
 * `host_output` and its `ready_output` is `host_ready_output`. Defining the slot to `host_outputv`
 * here and then including the fixture produces the one entry shape the other fixtures cannot cover:
 *
 *   * `.output   = host_output`     — the plain port-command callback;
 *   * `.outputv  = host_outputv`    — the vector port-command callback; and
 *   * `.ready_output = host_ready_output` — the write-readiness notification;
 *
 * all declared **at once**. The C's write path prefers the vector callback when both exist
 * (`if (drv->outputv) { … } else { … drv->output … }`, `erts/emulator/beam/io.c:1782`), and the
 * entry's `ready_output` still runs as the write-readiness notification
 * (`erl_port_task.c:1790-1797`) — the ordering this fixture exists to exercise. `host_outputv`
 * logs `outputv`, `host_output` logs `output` and `host_ready_output` emits `"W"` through
 * `driver_output`, so a host can prove which callbacks ran, in which order.
 *
 * `#include "host_driver.c"` resolves relative to this file's own directory (both fixtures live in
 * `tests/fixtures/`), so the variant compiles to the same code with just the one slot added; the
 * `driver_name` stays `"host_driver"`, which is why it is loaded under that name too.
 */
#define HOST_DRIVER_OUTPUTV_SLOT host_outputv
#include "host_driver.c"
