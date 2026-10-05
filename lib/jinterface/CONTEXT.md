# jinterface — Java↔Erlang Interface Library

## Intent
`lib/jinterface/` is the OTP **jinterface** application: a pure-Java library (`com.ericsson.otp.erlang`) that lets Java programs connect to an Erlang/OTP distributed node and exchange Erlang terms / Java representations of Erlang data types.
It is the Java counterpart of `erts` and `erl_interface` (`ei`) for distribution-protocol and term-format interoperability.
The layout diverges from the standard OTP app schema: there are **no Erlang runtime modules**; the "application" is a Java jar (`priv/OtpErlang.jar`) plus a generated `.app` resource.

## API Surface
- Java package `com.ericsson.otp.erlang` (sources under `java_src/com/ericsson/otp/erlang/`), enumerated by the `java_files` class list:
  - Communication: `OtpNode`, `OtpMbox`, `OtpSelf`, `OtpPeer`, `OtpLocalNode`, `OtpConnection`, `OtpCookedConnection`, `OtpEpmd`, `OtpMsg`, `OtpNodeStatus`.
  - Transports: `OtpTransport`, `OtpTransportFactory`, `OtpGenericTransportFactory`, `OtpSocketTransport`, `OtpSocketTransportFactory`, `OtpServerTransport`, `OtpServerSocketTransport`.
  - Term model: abstract `OtpErlangObject` plus `OtpErlangAtom/Binary/Bitstr/Boolean/Byte/Char/Double/Float/Int/Long/List/Map/String/Tuple/Pid/Port/Ref/Short/UInt/UShort/Fun/ExternalFun`.
  - Codec/support: `OtpInputStream`, `OtpOutputStream`, `OtpExternal`, `OtpMD5`.
  - Exceptions: `OtpException`, `OtpAuthException`, `OtpErlangException`, `OtpErlangDecodeException`, `OtpErlangExit`, `OtpErlangRangeException`.
- `java_src/com/ericsson/otp/erlang/jinterface.app.src` → generated `ebin/jinterface.app` (`{modules, []}`, empty `applications`/`runtime_dependencies` — a library app, not a supervised one).
- `java_src/com/ericsson/otp/erlang/jinterface.appup.src` → generated `ebin/jinterface.appup` (`{"%VSN%", [], []}`).
- `java_src/pom.xml.src` → generated `java_src/pom.xml` (Maven packaging, `Automatic-Module-Name: com.ericsson.otp.erlang`).
- `vsn.mk` — `JINTERFACE_VSN` (currently `1.16`); versioned independently of the OTP release.
- Built artifacts: `priv/OtpErlang.jar` and `priv/com/ericsson/otp/erlang/*.class`.

## Build
- `Makefile` → `SUB_DIRECTORIES = java_src doc`, then includes `../make/app_targets.mk`.
- `java_src/Makefile` → `SUB_DIRECTORIES = com/ericsson/otp/erlang`; generates `java_src/pom.xml` from `pom.xml.src` (sed `%VSN%`).
- `java_src/com/ericsson/otp/erlang/Makefile` compiles the `.java` files listed in `java_files` into `$(ERL_TOP)/lib/jinterface/priv/com/ericsson/otp/erlang/*.class`, jars them as `priv/OtpErlang.jar`, and generates `ebin/jinterface.app`/`ebin/jinterface.appup` (sed `%VSN%`).
- Uses `$(JAVA)` (= `@JAVAC@` from `make/otp.mk.in`) with `-Xlint -encoding UTF-8`; uses `jar` (`jar.exe` on win32); honours `SOURCE_DATE_EPOCH` (fixed jar date, requires JDK 17+).
- `release_spec` installs `java_src/**/*.java`, `priv/OtpErlang.jar`, and the `.app`/`.appup` into `$(RELEASE_PATH)/lib/jinterface-<VSN>/`.

## Constraints
- Requires a JDK (`javac` + `java`) at configure/build time (`--with-javac`/`--without-javac`, see repo-root `configure`); tests skip when no JVM is found.
- `ebin/` holds only `jinterface.app`/`jinterface.appup` — no `src/*.erl`; do not expect the pure-Erlang app schema.
- The compiled class list lives only in `java_src/com/ericsson/otp/erlang/java_files` — add new classes there, never in the Makefiles.
- `priv/` is git-ignored build output (jar + classes); `doc/assets/java/` (javadoc output) is also git-ignored.
- Wire-format/term-encoding compatibility is with the ERTS distribution protocol; Java source targets 1.6 (`.classpath` JavaSE-1.6, pom `source`/`target` 1.6).

## Dependencies
- JDK ≥ 1.6 (see `HOWTO/INSTALL.md`); JDK 17+ only for deterministic (`SOURCE_DATE_EPOCH`) jar builds.
- Runtime: a reachable Erlang/OTP distributed node and `epmd`; no OTP app dependencies (`runtime_dependencies` is empty).

## Notes for Agents
- This is a Java-binding app: the "source" is Java under `java_src/`, and the shipped "application" is `priv/OtpErlang.jar`.
- `java_src/pom.xml` and `ebin/jinterface.app`/`jinterface.appup` are generated (sed `%VSN%`) — edit the `.src`/`.in` sources, not the outputs.
- Eclipse project metadata (`.project`, `.classpath`, `.settings/`) is checked in for editing the Java sources; it is not part of the OTP build.
- `prebuild.keep` = `doc` (keep prebuilt docs) and `prebuild.skip` = `.` (skip prebuilt Java artifacts during bootstrap) — no prebuilt Java classes ship.
- `java_files` groups (`EXCEPTIONS`/`COMM`/`ERL`/`MISC`) are cosmetic; only the union `JAVA_FILES` matters.

## Test Strategy
- `test/` is a Common Test suite set (`MODULES = nc_SUITE jinterface_SUITE`, plus helper module `jitu`); installed as `$(RELEASE_PATH)/jinterface_test`.
- `jinterface_SUITE` drives real JVMs: `test/jitu.erl` builds a classpath from the suite dir + `code:lib_dir(jinterface)/priv/OtpErlang.jar` + `$CLASSPATH` and spawns `java`; it skips with `"Found no Java VM"` if `java` is absent, and `jitu:java/3,4,5` return an OS exit code.
- Java test drivers live in `test/jinterface_SUITE_data/*.java` (e.g. `NodePing`, `MboxSendReceive`, `MboxLinkUnlink`, `Maps`, `FunEquals`, `CoreMatchBind`) and `test/nc_SUITE_data/*.java`; they are compiled via their `Makefile.src` using configure-substituted `@JAVAC@`, `@jinterface_classpath@`, `@PS@`.
- `nc_SUITE` tests term encode/decode round-trips (pids/ports/refs/floats/unicode/funs/binary/compressed terms) against the Erlang side.
- Specs: `jinterface.spec` (all suites), `jinterface_smoke.spec` (`jinterface_SUITE:java_erlang_send_receive` only); `jinterface.cover`.
- Tests need both a JDK and a working `epmd`/distribution setup; the jar must be built first (it is part of the app build).

## Routing Table
- `java_src/` → Java library sources (`com/ericsson/otp/erlang/*.java`), the `java_files` class list, `jinterface.app.src`/`.appup.src`, `pom.xml.src`, and the build Makefiles.
- `test/` → Common Test suites (`jinterface_SUITE`, `nc_SUITE`), the JVM-launch helper `jitu`, Java drivers in `*_SUITE_data/`, and test specs/cover.
- `doc/` → ExDoc config (`docs.exs`), the user guide (`guides/jinterface_users_guide.md`), release notes (`notes.md`), and javadoc output under `assets/java/`.
- `ebin/` / `priv/` → build output only (`jinterface.app`/`appup`; `OtpErlang.jar` + classes).
- `../erl_interface/` → C counterpart (`ei`) implementing the same distribution/term formats (sibling — read-only, escalate writes to parent).

## See Also
- `../CONTEXT.md` (lib) for the standard OTP application schema; repo-root `CONTEXT.md` for build/release entry points.
- `../../erts/` for the runtime distribution protocol that jinterface speaks.
