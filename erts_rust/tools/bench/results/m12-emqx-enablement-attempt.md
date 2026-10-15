# m12 — `emqx` enablement attempt: why the honest skip stands

A bounded, first-hand attempt to make the `emqx` workload build on this host, run in a private
`$TMPDIR` copy of the read-only checkout with the driver's own toolchain gate bypassed. The attempt
**failed**: EMQX-enterprise is not buildable in this sandbox. The `emqx` workload therefore keeps its
honest `skipped(emqx-requires-otp-28)` — it reports no number, and none is fabricated.

* Host: `erl` = **OTP 29.1.1 / erts-17.1**; `mix`/`elixir` present; **no** `automake`, **no** `libtool`,
  **no** `cmake`, **no** Kerberos headers.
* Target: the EMQX checkout staged into a `$TMPDIR` scratch copy (`HOME`/`MIX_HOME`/`HEX_HOME` redirected
  into that copy), driven through its **real** build entry point `make emqx-enterprise-compile`
  (the `Makefile`'s `PROFILE ?= emqx-enterprise` target: bootstrap the EMQX `rebar3` fork,
  `mix deps.get`, `mix compile`).
* Read-only source: every command below ran in the `$TMPDIR` copy; the checkout was never written to.

## The exact command run

```sh
# Stage the checkout into a scratch copy (the driver's ScratchDir::copy_tree equivalent)
# and drive the real umbrella build with a writable HOME inside the copy.
make emqx-enterprise-compile     # cwd = the $TMPDIR copy; run via the repo's own ./mix wrapper
```

After each blocker the failing step was re-run with the minimal fix for the *previous* blocker applied
(see the ladder), so the next blocker is the one the build actually reached — never an assumption.

## The verified ladder of blockers

Each entry is the **exact** failing output of the command at that rung.

**1. `:ezstd`'s NIF makefile under OTP 29 — the declared-pin blocker.**
`deps/ezstd/c_src/nif.mk:19` generates the NIF's `env.mk` with
`erl -noshell -s init stop -eval "file:write_file(\"$(C_SRC_ENV)\", io_lib:format(...)), halt()."`.
On OTP 29 the `-s init stop` halts *before* the `-eval` runs, so the file is never written:

```
Error! Failed to eval: file:write_file("...", ...)
make[2]: *** No rule to make target '.../deps/ezstd/c_src/env.mk'.  Stop.
** (Mix) Could not compile dependency :ezstd
```

Verified directly and independently: `erl -noshell -s init stop -eval 'io:format("X~n")'` exits **0**
with **no output**; putting `-eval` first makes the same expression print. (This is the mechanism the
`emqx` gate detects cheaply; `otp_gate_note` names it as blocker 1.)

> The literal, reproducible patch for this one-line idiom change is committed as
> `results/m12-emqx-ezstd-env-mk.patch` — a real unified diff (with the exact source tag/line, the
> apply-after-`mix deps.get` note and the "insufficient alone" warning in its header).

**2. Patching that one idiom: the build then fails in `:jq`.**
Reordering the `ezstd` idiom to `erl -noshell -eval …` gets past `:ezstd`; the build then fails while
building `:jq`, whose NIF runs `autoreconf -fi`:

```
Can't exec "aclocal": No such file or directory
```

(The host has no `automake`.)

**3. Providing `automake` + `libtool` + `cmake` from the Nix store moves the failure inside `jq`'s
`configure`:**

```
src/Makefile.am:24: error: Libtool library used but 'LIBTOOL' is undefined
```

(libtool's `m4` macros must be on `ACLOCAL_PATH`.)

**4. With `ACLOCAL_PATH=<libtool>/share/aclocal`, `jq`'s generated `configure` dies on a
nix-shell-injected, non-existent shell path:**

```
./configure: line 136: /nix/store/<...>-bash-5.3p9/bin/bash: No such file or directory
```

**5. Using the three store `bin` dirs directly on `PATH` (`SHELL=/bin/sh CONFIG_SHELL=/bin/sh`, no
`nix shell`) finally builds `:jq` fully** — and the build then fails in `:sasl_auth`:

```
deps/sasl_auth/c_src/sasl_auth.c:3:10: fatal error: krb5.h: No such file or directory
```

(No Kerberos headers.)

## Conclusion

EMQX-enterprise is **not buildable in this sandbox**. The binding constraints are a **missing host build
toolchain / system libraries** — `automake`, `libtool`, `cmake`, `krb5` headers and almost certainly
more behind `:sasl_auth` — on top of the **OTP-29 `:ezstd` idiom** that the checkout's own pin
(OTP 28.5.0.7-2) avoids by construction. The checkout's own CI builds it in a prepared Docker image, not
on a bare host.

**The `:ezstd` idiom patch alone is insufficient.** Rung 2 shows the build simply walking on to `:jq`;
rungs 3–5 show each subsequent host-tooling gap, ending at `krb5.h`. Fixing the idiom does not add the
missing compiler/autotools/library surface, and the driver must not vendor a prepared image or shell out
to a container runtime it does not have.

So the driver **keeps its cheap, honest skip**: it detects the OTP-newer-than-pin mismatch up front (no
minutes spent discovering a doomed build) and records `skipped(emqx-requires-otp-28)` with a note that
names the exact pin, the host toolchain, **and both** verified blockers (the `:ezstd` `env.mk` idiom and
the missing host build tooling/libs). The skip reason string is unchanged and pinned by the workload's
tests; the full measurement path still runs unchanged the moment a matching toolchain is present.

**No number is fabricated:** nothing in this attempt produced a benchmark result, and none is recorded
anywhere. The `emqx` row remains the honest skip.
