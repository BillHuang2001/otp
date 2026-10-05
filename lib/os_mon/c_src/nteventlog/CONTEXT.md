# nteventlog — Windows Event Log port program

## Intent
Windows-only helper program (`nteventlog.exe`) that tails the Win32 Event Log and forwards new records to Erlang over stdin/stdout.
It backs the `os_sup` service on Windows: the Erlang side is `lib/os_mon/src/nteventlog.erl` (started by `os_mon.erl` for `{win32,_}` and by `os_sup.erl`).
The program keeps per-identifier "last record read" state in the registry so that, when the same identifier is reused, each event-log record is delivered at most once.
Pure Win32 C — it does not link the Erlang runtime; it only uses the port entry point (see below).

## API Surface (files)
- `elog_main.c` — `main()`: reads the identifier from `argv[1]` (defaults to `DEFAULT_IDENT` `"DefaultIdent"`, `argv[0]` is the exe path), opens each log, replays unseen records, then waits for new ones.
- `elog_global.h` — common includes plus Win32 setup (`STRICT`, `WIN32_LEAN_AND_MEAN`), size macros (`SMALLBUFSIZ` 512, `BIGBUFSIZ` 2048, `MAX_PARAM_STRINGS`, `MAX_FACILITY_NAME`), the `MessageFiles` struct and `DEFAULT_LANGID`.
- `elog_util.{c,h}` — `category_tab` (the three logs `System`/`Application`/`Security`) with `NUM_CATEGORIES`, the severity table + `lookup_severity()`, `output_error()` (FormatMessage of a `GetLastError()` code to stderr), and `my_malloc()` which is injected as `malloc` via a macro.
- `elog_registry.{c,h}` — `RegKeys` struct plus `get_regkeys()`/`set_regkeys()`/`create_regkeys()` for the reader state, and `get_messagefiles()` which resolves the EventMessageFile/ParameterMessageFile DLLs.
- `elog_format.{c,h}` — `format_message()`: loads the message DLLs and produces the human-readable text (with `expand_message()` resolving `%%NNN` placeholders).
- `elog_pipe_stdin.{c,h}` — stdin abstraction: reader thread + manual-reset event + critical section when stdin is a pipe, plain console handle when it is a console; exports `setup_pipe_stdin()`, `console_stdin()`, `get_stdin_event()`, `read_pipe_stdin()`, `peek_pipe_stdin_eof()`.

## Wire protocol (with `nteventlog.erl`)
- Framing: 2-byte big-endian length prefix in both directions — the Erlang side opens the port with `open_port({spawn, Command}, [{packet,2}])` on `"<privdir>/bin/nteventlog.exe" <identifier>`.
- Outgoing record (pipe mode): `"%dH%s%dH%s%dH%s%dH%s%dH%s"` — five Hollerith-prefixed fields, in order Time, Category, Facility, Severity, Message.
  - Time is `event->TimeGenerated` as a decimal `DWORD`; `nteventlog.erl:parse_log/1` (`holl_time/1`) re-splits that decimal string into `{High, Low, 0}` (last 6 digits = `Low`).
  - Category is the log name (`System`/`Application`/`Security`), Facility is the event's source string taken right after the `EVENTLOGRECORD` header.
- Incoming: Erlang must send `{command, "A"}` (ACK) after each record; the C side reads the 2-byte length, then the payload, and fails if it is not exactly `"A"`. No ACK ⇒ the program exits and the port dies.
- Console mode (stdin is a console, e.g. a manual test run) uses the human format `"%s [%s] %s, %s: %s\n"` (time, facility, category, severity, message) and requests no ACK.
- Termination: EOF on stdin (pipe closed / `ERROR_BROKEN_PIPE`) or `^Z` twice on a console makes the program exit 0.

## Constraints
- Windows only — `../Makefile.in` only compiles these files in the `ifeq ($(findstring win32,$(TARGET)),win32)` branch; MSVC CRT and the Windows SDK (advapi32 event-log + registry APIs) are required.
- The program is linked with `-entry:erl_port_entry` against `$(ERL_TOP)/erts/obj/$(TARGET)/port_entry.o` (source: `erts/etc/win32/port_entry.c`), so it inherits the console-control handling that keeps it alive across service logoff/shutdown (it calls `mainCRTStartup()`, which then runs `main()`).
- Requires a user allowed to read `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\EventLog\...` and to open the event logs (normally Administrator).
- `NUM_CATEGORIES` (3) in `elog_util.h` must be updated together with `category_tab`.
- Any `#include "elog_util.h"` redefines `malloc` as `my_malloc` (fatal on OOM) — files that want plain `malloc` (e.g. `elog_pipe_stdin.c`) include only `elog_global.h`.

## Design Decisions
- Restart/dedup state lives in the registry (`HKEY_LOCAL_MACHINE\SOFTWARE\Ericsson\Erlang\EventLogReader\1.0\<identifier>\<logname>`, values `LatestRecord`/`LatestTime`, auto-created) rather than in a file, and is only written after a record has been ACKed.
- Message text is resolved by loading the source's message DLLs (`LoadLibraryEx(..., DONT_RESOLVE_DLL_REFERENCES)`) and calling `FormatMessage` twice: once from the event module, once from the string, with `%%NNN` parameters expanded from the ParameterMessageFile.
- Log change notification uses `NotifyChangeEventLog` + `WaitForMultipleObjects` over the per-log events and the stdin event; a `RETRY_TIMEOUT` (1000 ms) retry covers the inherent race between the notification and the log becoming readable.
- `ERROR_EVENTLOG_FILE_CHANGED` sets the `reopen_event_log` flag, which makes the main loop close and reopen all logs.
- Stdin is abstracted because a pipe handle is always "signaled"; asynchronous (overlapped) I/O is not available for anonymous pipes, hence the reader thread.

## Known Issues
- Standard severity strings are fixed in `severity_tab`: `Error`, `Warning`, `Informational`, `Audit_Success`, `Audit_Faulure` (sic — typo also delivered to Erlang), `Severity_Unknown`.
- Message text is capped at `BIGBUFSIZ` (2048); anything that fails to format is replaced by the literal `[EventLogReader unable to format message text]`.
- `elog_pipe_stdin.h` documents that pipe stdin buffering grows forever — unsolicited stdin data would accumulate unbounded.
- Naming is legacy-confusing: the `facility_name` field / `category` parameter actually hold the *log* name (`System`/`Application`/`Security`), while `fac`/the `facility` argument of `get_messagefiles()` is the event *source*.
- `main()`'s `rk` array is sized `NUM_CATEGORIES` and never grows, so extra registry subkeys are silently ignored.

## Notes for Agents
- There is no `load_if_possible()` here; that helper lives in the sibling `../win32sysinfo.c`. This program's process entry point is `erl_port_entry`.
- `../Makefile` is generated from `../Makefile.in`: adding/removing a C file means editing both the `C_FILES` list (for release install) and `EVLOG_OBJECTS`.
- Objects are built into the shared `../priv/obj/$(TARGET)` via the pattern rule `$(OBJDIR)/%.o: nteventlog/%.c`; the executable goes to `../priv/bin/$(TARGET)` (i.e. `os_mon/priv/bin/win32/`), and `release_spec` flattens it into `$(RELSYSDIR)/priv/bin/nteventlog.exe`, which matches `code:priv_dir(os_mon)/bin/nteventlog.exe` used by the Erlang side.
- `lib/os_mon/c_src/win32/` is git-ignored legacy build output (repo-root `.gitignore`), not a source directory.

## Test Strategy
- No Common Test suite covers this program (`lib/os_mon/test/` has none for `nteventlog`); behaviour is exercised indirectly through `os_sup`/`os_mon` suites only on Windows.
- The console (`console_stdin()`) code path exists so the program can be run by hand as `nteventlog.exe <identifier>` for manual verification.

## See Also
- `../CONTEXT.md` — sibling C programs of `os_mon` (`cpu_sup`, `memsup`, `win32sysinfo`, `mod_syslog`, `ferrule`).
- `../../src/nteventlog.erl` — Erlang peer that spawns and talks to this program; `../../src/os_sup.erl`, `../../src/os_mon.erl` — how it is started on Windows.
