//! Module-run support: compile an embedded Erlang module with the system `erlc`, and (for a
//! NIF case) its C fixtures with the host C compiler.
//!
//! The module-run corpus cases ([`crate::corpus::CaseKind::Module`]) compare a *compiled*
//! `.beam` executed by both runtimes. The module source is embedded in this crate; this
//! module writes it to a unique temporary directory and compiles it with the system
//! `erlc`, returning a [`CompiledModule`] that removes the directory when dropped.
//!
//! `erlc` is resolved from `$ERLC` if set (which must name an existing executable, else a
//! loud [`Error::Env`], mirroring `$ERTS_ORACLE`/`$BEAM_RT`), otherwise by scanning
//! `$PATH`. When no `erlc` exists, [`resolve_erlc`] returns `Ok(None)`: the harness then
//! **skips** the module-run case loudly (never a silent pass). A present `erlc` that fails
//! to compile is a genuine tool failure (a loud error), not a skip.
//!
//! # C/NIF fixtures (contract A.3)
//!
//! A module-run case may carry a [`NifFixture`] — the C (or plain-text) artifacts that must
//! sit **next to the compiled `.beam`** for the module to load them at run time. The module
//! self-locates them with `filename:dirname(code:which(?MODULE))`, so neither runtime needs
//! a new argv flag or a `-pa` addition, and both runtimes are driven with the **same** argv
//! shape as any other module-run case. [`build`] writes them into that case's temp dir.
//!
//! A fixture compiled from C needs two more host facilities, both resolved here:
//!
//! * a **C compiler** — `$CC` if set (an existing executable, or a command on `$PATH`), else
//!   `cc` on `$PATH` ([`resolve_cc`]);
//! * the **`erl_nif.h` include directory** the library is compiled against
//!   ([`resolve_erl_include`]) — the *loading* runtime's header, probed first from the
//!   oracle, then from the `erts-*` install of the `erl` on `$PATH`, then from the standard
//!   installation roots.
//!
//! Either one missing ⇒ [`Build::Skipped`] with a loud reason ([`MISSING_CC_REASON`],
//! [`MISSING_ERL_NIF_REASON`]) — the case is never run, never reported as agreement, and the
//! harness never crashes. A *present* compiler that fails is a genuine tool failure (a loud
//! [`Error`]), not a skip. A plain-text fixture ([`NifFixture::TextFileSo`]) needs neither
//! facility: it is written verbatim.
//!
//! # Compile cache (iteration speed)
//!
//! The corpus compiles the *same* handful of embedded sources once per module-run case
//! (~50 cases, 2 distinct sources), so [`compile`] memoizes the `erlc` output in a
//! **process-local cache** keyed by `(erlc, module, source)`. A cache hit *materializes* a
//! fresh per-case temp dir from the cached `.beam` bytes without spawning `erlc` again —
//! the bytes, the argv and therefore every observable result are identical, only the
//! redundant compiler process disappears. Each call still owns its own directory, removed
//! by [`CompiledModule`]'s `Drop` exactly as before. A **C-compiled fixture** is memoized
//! the same way, keyed by its `(cc, include, name, c source)` identity: several NIF cases
//! share one library, so the second one writes the cached `.so` bytes instead of spawning
//! `cc` again.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::error::{Error, Result};
use crate::runtime::{is_executable, output_with_retry};

/// The reason recorded when no `erlc` could be found (the harness skips the case with it).
pub const MISSING_ERLC_REASON: &str = "no erlc compiler found: $ERLC is unset and no 'erlc' \
     was found on $PATH (the module-run cases need the system compiler)";

/// The reason recorded when no C compiler could be found (the harness skips a NIF case that
/// needs a `.so` compiled from C with it).
pub const MISSING_CC_REASON: &str = "no C compiler found: $CC is unset and no 'cc' was found \
     on $PATH (a NIF module-run case needs one to build its .so)";

/// The reason recorded when no `erl_nif.h` include directory could be located (the harness
/// skips a NIF case that needs a `.so` compiled from C with it).
pub const MISSING_ERL_NIF_REASON: &str = "no erl_nif.h include directory found: the oracle's \
     code:lib_dir(erts,include) holds no header, no `include` sits beside the erts-* \
     directory of the `erl` on $PATH, and none of the standard roots ($ERL_TOP, \
     /usr/lib/erlang, /usr/local/lib/erlang, /usr/lib64/erlang, /opt/erlang) has one";

/// Resolve `erlc` from the real environment.
///
/// `Ok(None)` means "no compiler to compile a module with" — the caller must **skip** the
/// module-run case, never report agreement.
pub fn resolve_erlc() -> Result<Option<PathBuf>> {
    resolve_erlc_from(std::env::var_os("ERLC"), std::env::var_os("PATH"))
}

/// Resolve `erlc` from explicit inputs (the injectable core of [`resolve_erlc`]).
///
/// * `erlc` — the value of `$ERLC`, if set. It must name an existing executable,
///   otherwise this is a loud [`Error::Env`].
/// * `path_var` — the value of `$PATH` used to locate `erlc` when `$ERLC` is unset.
pub fn resolve_erlc_from(erlc: Option<OsString>, path_var: Option<OsString>) -> Result<Option<PathBuf>> {
    if let Some(value) = erlc {
        let program = PathBuf::from(&value);
        if !is_executable(&program) {
            return Err(Error::env(format!(
                "$ERLC is set to '{}', which is not an existing executable",
                program.display()
            )));
        }
        return Ok(Some(program));
    }

    Ok(path_var
        .as_deref()
        .and_then(|path_var| find_on_path(erlc_program_names(), path_var)))
}

/// Resolve the C compiler from the real environment: `$CC` if set, else `cc` on `$PATH`.
///
/// `Ok(None)` means "no C compiler to build a NIF `.so` with" — the caller must **skip** a
/// C-compiled fixture, never run it against a library that does not exist.
pub fn resolve_cc() -> Result<Option<PathBuf>> {
    resolve_cc_from(std::env::var_os("CC"), std::env::var_os("PATH"))
}

/// Resolve the C compiler from explicit inputs (the injectable core of [`resolve_cc`]).
///
/// * `cc` — the value of `$CC`, if set. It must name an existing executable **or** a command
///   found on `$PATH` (the conventional `CC=gcc`), otherwise this is a loud [`Error::Env`].
/// * `path_var` — the value of `$PATH`, scanned for `cc` when `$CC` is unset.
pub fn resolve_cc_from(cc: Option<OsString>, path_var: Option<OsString>) -> Result<Option<PathBuf>> {
    let path_var = path_var.as_deref();
    let Some(value) = cc else {
        return Ok(path_var.and_then(|path_var| find_on_path(cc_program_names(), path_var)));
    };

    let program = PathBuf::from(&value);
    if is_executable(&program) {
        return Ok(Some(program));
    }
    // `$CC` conventionally names a command (`CC=gcc`) rather than a path, so a bare name is
    // looked up on `$PATH` before the override is rejected as unusable.
    let name = program.to_string_lossy().into_owned();
    if let Some(found) = path_var.and_then(|path_var| find_on_path(&[name.as_str()], path_var)) {
        return Ok(Some(found));
    }
    Err(Error::env(format!(
        "$CC is set to '{}', which is neither an existing executable nor a command on $PATH",
        program.display()
    )))
}

/// The first executable named any of `names` in the `$PATH` value `path_var`.
fn find_on_path(names: &[&str], path_var: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let candidate = dir.join(name);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The header the include-directory search keys on.
const ERL_NIF_HEADER: &str = "erl_nif.h";

/// Locate the include directory holding the host `erl_nif.h` — the header a NIF `.so` is
/// compiled against — or `None` when there is none.
///
/// No single probe is reliable on every installation, so three are tried in order and the
/// first directory that actually holds a **readable** `erl_nif.h` wins:
///
/// 1. ask the **oracle** (`$ERTS_ORACLE`, else `erl`) for `code:lib_dir(erts, include)` — the
///    authority, since the library is dlopened by that runtime too;
/// 2. canonicalize the `erl` on `$PATH` and take the `include` sibling of its `erts-*`
///    directory (the standard layout: this sandbox's probe 1 answers with a path that does
///    not exist, and probe 2 is what makes the NIF cases work here);
/// 3. probe the standard installation roots (`$ERL_TOP`, `/usr/lib/erlang`, …).
///
/// The answer is cached process-locally: it cannot change during a run, and probe 1 spawns a
/// whole `erl` (which would otherwise cost ≈100 ms *per case*).
pub fn resolve_erl_include() -> Option<PathBuf> {
    static INCLUDE: OnceLock<Option<PathBuf>> = OnceLock::new();
    INCLUDE.get_or_init(detect_erl_include).clone()
}

/// The uncached include-directory probe (see [`resolve_erl_include`]).
fn detect_erl_include() -> Option<PathBuf> {
    oracle_include_dir()
        .or_else(path_erl_include_dir)
        .or_else(standard_root_include_dir)
}

/// Probe 1: `code:lib_dir(erts, include)` as answered by the oracle runtime.
fn oracle_include_dir() -> Option<PathBuf> {
    let program = std::env::var_os("ERTS_ORACLE").unwrap_or_else(|| OsString::from("erl"));
    let mut cmd = Command::new(&program);
    cmd.args([
        "-noshell",
        "-eval",
        "io:format(\"~s\",[code:lib_dir(erts,include)])",
        "-s",
        "init",
        "stop",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    // A probe that cannot run is not an error here: the callers fall through to the next
    // probe, and if every probe fails the case is *skipped loudly* (never silently run).
    let output = output_with_retry(&mut cmd).ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    is_include_dir(&dir).then_some(dir)
}

/// Probe 2: the `include` sibling of the `erts-*` directory of the `erl` on `$PATH`.
fn path_erl_include_dir() -> Option<PathBuf> {
    let erl = which_on_path("erl")?.canonicalize().ok()?;
    let bin = erl.parent()?;
    let erts = bin.parent()?;
    if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
        return None;
    }
    let include = erts.join("include");
    is_include_dir(&include).then_some(include)
}

/// The first `name` on `$PATH`, whether or not it is executable (used only to locate an
/// installation layout, so the file is canonicalized before anything is run).
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Probe 3: the standard installation roots.
fn standard_root_include_dir() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(top) = std::env::var_os("ERL_TOP") {
        roots.push(PathBuf::from(top));
    }
    for base in [
        "/usr/lib/erlang",
        "/usr/local/lib/erlang",
        "/usr/lib64/erlang",
        "/opt/erlang",
    ] {
        roots.push(PathBuf::from(base));
    }
    roots.into_iter().find_map(|root| search_installation_root(&root))
}

/// Search one installation root — and its `lib/erlang` sub-root — for an `include` directory
/// holding `erl_nif.h`: `<root>/include`, `<root>/erts-*/include`, `<root>/lib/erlang/include`.
fn search_installation_root(root: &Path) -> Option<PathBuf> {
    let direct = root.join("include");
    if is_include_dir(&direct) {
        return Some(direct);
    }
    let lib_erlang = root.join("lib").join("erlang");
    for base in [root.to_path_buf(), lib_erlang.clone()] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("erts-") {
                continue;
            }
            let include = entry.path().join("include");
            if is_include_dir(&include) {
                return Some(include);
            }
        }
    }
    let include = lib_erlang.join("include");
    is_include_dir(&include).then_some(include)
}

/// True when `dir` holds a readable `erl_nif.h`.
fn is_include_dir(dir: &Path) -> bool {
    dir.join(ERL_NIF_HEADER).is_file()
}

/// A module compiled into a temporary directory, removed when dropped.
#[derive(Debug)]
pub struct CompiledModule {
    dir: PathBuf,
    beam: PathBuf,
}

impl CompiledModule {
    /// The temporary directory holding the compiled output (passed to the runtimes as
    /// `-pa <dir>` and `<dir>/<module>.beam`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The compiled `<dir>/<module>.beam`.
    pub fn beam(&self) -> &Path {
        &self.beam
    }
}

impl Drop for CompiledModule {
    fn drop(&mut self) {
        // Best-effort cleanup: a failure to remove the temp dir must not mask the case's
        // result (the directory is unique and under the system temp dir).
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Compile `source` (declaring a module named `module`) with `erlc` into a fresh temp dir.
///
/// A missing `erlc` binary or a compile failure is a loud [`Error`] (the caller decides
/// whether to run the case). On any failure the temp directory is removed before
/// returning.
///
/// The `erlc` invocation is memoized process-locally by `(erlc, module, source)`: a repeat
/// compile of the same source reuses the cached `.beam` bytes (see the module docs). The
/// returned [`CompiledModule`] always names its **own** fresh temp dir and removes it on
/// drop, whether it came from `erlc` or from the cache.
pub fn compile(erlc: &Path, source: &str, module: &str) -> Result<CompiledModule> {
    let key = CacheKey {
        erlc: erlc.to_path_buf(),
        module: module.to_string(),
        source: source.to_string(),
    };

    if let Some(beam) = cached_beam(&key) {
        return materialize(module, source, &beam);
    }

    let compiled = compile_with_erlc(erlc, source, module)?;
    // Cache the artifact so the next case with the same source skips the compiler. A beam
    // we cannot read afterwards cannot be run by either runtime either, so surface it loudly.
    let beam = std::fs::read(compiled.beam()).map_err(|source| {
        Error::io(
            format!(
                "failed to read the compiled beam '{}'",
                compiled.beam().display()
            ),
            source,
        )
    })?;
    store_beam(key, beam);
    Ok(compiled)
}

/// The uncached compile: run the real `erlc` into a fresh temp dir.
fn compile_with_erlc(erlc: &Path, source: &str, module: &str) -> Result<CompiledModule> {
    let dir = fresh_temp_dir(module)?;
    let source_path = dir.join(format!("{module}.erl"));
    write_into_temp(&dir, &source_path, source.as_bytes())?;

    // `output_with_retry` absorbs the transient `ETXTBSY` fork/exec race: `erlc` may be a
    // freshly written wrapper script (e.g. a test stub), so a concurrent `fork()` in another
    // test thread can transiently make `exec` fail with "Text file busy".
    let mut cmd = Command::new(erlc);
    cmd.arg("-o")
        .arg(&dir)
        .arg(&source_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = output_with_retry(&mut cmd).map_err(|source| {
        Error::io(
            format!("failed to execute erlc '{}'", erlc.display()),
            source,
        )
    })?;

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(Error::io(
            format!(
                "erlc failed to compile '{}' (exit {:?}): {}",
                source_path.display(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            std::io::Error::new(std::io::ErrorKind::Other, "erlc reported a compile failure"),
        ));
    }

    let beam = dir.join(format!("{module}.beam"));
    if !beam.is_file() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(Error::io(
            format!(
                "erlc produced no '{}' for module {module}",
                beam.display()
            ),
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing .beam output"),
        ));
    }

    Ok(CompiledModule { dir, beam })
}

/// A cache hit: write the *cached* `beam` bytes (plus the source, matching an `erlc`-built
/// directory) into a fresh temp dir without spawning `erlc`.
fn materialize(module: &str, source: &str, beam: &[u8]) -> Result<CompiledModule> {
    let dir = fresh_temp_dir(module)?;
    let source_path = dir.join(format!("{module}.erl"));
    write_into_temp(&dir, &source_path, source.as_bytes())?;
    let beam_path = dir.join(format!("{module}.beam"));
    write_into_temp(&dir, &beam_path, beam)?;
    Ok(CompiledModule {
        dir,
        beam: beam_path,
    })
}

/// The C/NIF artifacts a module-run case needs materialized **next to its `.beam`**.
///
/// Both variants hold only `'static` data (so a [`crate::corpus::Case`] stays `Copy`); the
/// work — compiling, writing — happens in [`build`]. The module loads the artifact by
/// building the path at run time (`filename:dirname(code:which(?MODULE))` ++ `"/<name>"`),
/// which is why the artifacts must share the compiled module's temp dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NifFixture {
    /// Compile `c_source` into `<dir>/<name>.so` with the host C compiler and the host
    /// `erl_nif.h` — a real NIF library the module can `erlang:load_nif/2`.
    CompiledLibrary {
        /// The library's base name. Both runtimes append `.so` themselves, so the module
        /// must load the path **without** the suffix (and the file is `<name>.so`).
        name: &'static str,
        /// The C source of the library, compiled fresh into the case's temp dir (never read
        /// from the repository at run time).
        c_source: &'static str,
    },
    /// Write `contents` verbatim to `<dir>/<name>.so`: a file that is *named* like a library
    /// but is not one, so `erlang:load_nif/2` fails without a C compiler being involved.
    TextFileSo {
        /// The file's base name (the file is `<name>.so`).
        name: &'static str,
        /// The literal file contents.
        contents: &'static str,
    },
}

/// The outcome of [`build`]ing a module-run case's artifacts.
#[derive(Debug)]
pub enum Build {
    /// The module (and, when the case carries one, its C/NIF fixture) is ready under the
    /// returned temp dir, which removes itself on drop.
    Compiled(CompiledModule),
    /// A host facility is missing, so the case was **not** built: it must be reported as
    /// `Verdict::Skipped` with this loud reason — never as agreement, never as a crash.
    Skipped(&'static str),
}

/// Build everything a module-run case needs: the embedded module, plus (when the case
/// carries one) its C/NIF fixture, materialized into the **same** temp dir.
///
/// A missing host facility is [`Build::Skipped`]; a present `erlc`/`cc` that fails is a
/// genuine tool failure (a loud [`Error`]). The fixtures are materialized *after* the module
/// compiles, into the directory the compiled `.beam` lives in, so the module can find them
/// through `code:which(?MODULE)`.
pub fn build(source: &str, module: &str, nif: Option<NifFixture>) -> Result<Build> {
    // The C toolchain is only resolved — and only *needed* — by a fixture compiled from C: a
    // case with no fixture, or with a verbatim one, must still run on a machine that has no C
    // compiler, and an unusable `$CC` must not break an unrelated case.
    let (cc, include) = match nif {
        Some(NifFixture::CompiledLibrary { .. }) => (resolve_cc()?, resolve_erl_include()),
        _ => (None, None),
    };
    build_with(resolve_erlc()?, cc, include, source, module, nif)
}

/// The tool-injection core of [`build`] (`None` means "that host tool is absent").
fn build_with(
    erlc: Option<PathBuf>,
    cc: Option<PathBuf>,
    include: Option<PathBuf>,
    source: &str,
    module: &str,
    nif: Option<NifFixture>,
) -> Result<Build> {
    let Some(erlc) = erlc else {
        return Ok(Build::Skipped(MISSING_ERLC_REASON));
    };

    // Resolve the fixture *before* compiling the module: a missing compiler must skip the
    // case, not produce a module nothing can load.
    let fixture = match nif {
        None => None,
        Some(NifFixture::TextFileSo { name, contents }) => Some(Fixture::Text { name, contents }),
        Some(NifFixture::CompiledLibrary { name, c_source }) => {
            let Some(cc) = cc else {
                return Ok(Build::Skipped(MISSING_CC_REASON));
            };
            let Some(include) = include else {
                return Ok(Build::Skipped(MISSING_ERL_NIF_REASON));
            };
            Some(Fixture::Library {
                cc,
                include,
                name,
                c_source,
            })
        }
    };

    let compiled = compile(&erlc, source, module)?;
    if let Some(fixture) = &fixture {
        // A failure here drops `compiled`, which removes the half-built temp dir with it.
        fixture.materialize(compiled.dir())?;
    }
    Ok(Build::Compiled(compiled))
}

/// A fixture with its host tools already resolved (see [`build`]); the tool resolution
/// happens before the module is compiled, so a skip never runs `erlc`.
enum Fixture {
    /// `<name>.so` holding `contents` verbatim.
    Text {
        name: &'static str,
        contents: &'static str,
    },
    /// `<name>.so` compiled from `c_source` with `cc` against the `erl_nif.h` in `include`.
    Library {
        cc: PathBuf,
        include: PathBuf,
        name: &'static str,
        c_source: &'static str,
    },
}

impl Fixture {
    /// Materialize the fixture into `dir` (a compiled module's temp dir).
    fn materialize(&self, dir: &Path) -> Result<()> {
        match self {
            Fixture::Text { name, contents } => {
                write_into_temp(dir, &so_path(dir, name), contents.as_bytes())
            }
            Fixture::Library {
                cc,
                include,
                name,
                c_source,
            } => {
                let key = NifCacheKey {
                    cc: cc.clone(),
                    include: include.clone(),
                    name,
                    c_source,
                };
                if let Some(bytes) = cached_shared_object(&key) {
                    return write_into_temp(dir, &so_path(dir, name), &bytes);
                }
                let library = compile_shared_object(dir, cc, include, name, c_source)?;
                // Cache the bytes so the next case with the same fixture skips `cc`; the
                // artifact handed to a case is still written into its own temp dir, so the
                // cache cannot change what either runtime observes. A library we cannot read
                // back is one no runtime could load either, so surface it loudly.
                let bytes = std::fs::read(&library).map_err(|source| {
                    Error::io(
                        format!("failed to read the compiled library '{}'", library.display()),
                        source,
                    )
                })?;
                store_shared_object(key, bytes);
                Ok(())
            }
        }
    }
}

/// `<dir>/<name>.so` — the file both runtimes' `erlang:load_nif/2` appends `.so` to, so the
/// *module* asks for the suffix-less `<dir>/<name>`.
fn so_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.so"))
}

/// Compile `c_source` into `<dir>/<name>.so` with `cc -shared -fPIC -I<include>`.
fn compile_shared_object(
    dir: &Path,
    cc: &Path,
    include: &Path,
    name: &str,
    c_source: &str,
) -> Result<PathBuf> {
    let source_path = dir.join(format!("{name}.c"));
    write_into_temp(dir, &source_path, c_source.as_bytes())?;
    let library = so_path(dir, name);

    // `output_with_retry` absorbs the transient `ETXTBSY` fork/exec race, exactly as the
    // `erlc` spawn does (`cc` may itself be a freshly written wrapper script).
    let mut cmd = Command::new(cc);
    cmd.arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(&library)
        .arg(&source_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = output_with_retry(&mut cmd).map_err(|source| {
        Error::io(
            format!("failed to execute the C compiler '{}'", cc.display()),
            source,
        )
    })?;

    if !output.status.success() {
        return Err(Error::io(
            format!(
                "the C compiler '{}' failed to build '{}' (exit {:?}): {}",
                cc.display(),
                library.display(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "the C compiler reported a build failure",
            ),
        ));
    }

    if !library.is_file() {
        return Err(Error::io(
            format!("the C compiler produced no '{}'", library.display()),
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing .so output"),
        ));
    }

    Ok(library)
}

/// Create a fresh, empty temp dir for `module` (removing a stale one of the same name left
/// by an earlier crashed run).
fn fresh_temp_dir(module: &str) -> Result<PathBuf> {
    let dir = unique_temp_dir(module);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|source| {
        Error::io(
            format!("failed to create the module temp dir '{}'", dir.display()),
            source,
        )
    })?;
    Ok(dir)
}

/// Write `bytes` to `path`; on failure remove the whole temp `dir` and fail loudly.
fn write_into_temp(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    if let Err(source) = std::fs::write(path, bytes) {
        let _ = std::fs::remove_dir_all(dir);
        return Err(Error::io(
            format!("failed to write '{}'", path.display()),
            source,
        ));
    }
    Ok(())
}

/// The identity of a compiled artifact: everything that determines its `.beam` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    erlc: PathBuf,
    module: String,
    source: String,
}

/// The process-local compile cache: `(erlc, module, source) -> compiled `.beam` bytes.
///
/// It only ever short-circuits a redundant `erlc` spawn; the artifact handed to a case is
/// still written into that case's own temp dir, so the cache cannot change what is observed.
fn compile_cache() -> &'static Mutex<HashMap<CacheKey, Vec<u8>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Vec<u8>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cache lock, recovering from a poisoned lock (only `HashMap` reads/inserts happen
/// under it, so a recovered map is always consistent).
fn cache_lock<K: Eq + Hash>(
    cache: &'static Mutex<HashMap<K, Vec<u8>>>,
) -> MutexGuard<'static, HashMap<K, Vec<u8>>> {
    cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A copy of the cached `.beam` bytes for `key`, or `None` on a miss.
fn cached_beam(key: &CacheKey) -> Option<Vec<u8>> {
    cache_lock(compile_cache()).get(key).cloned()
}

/// Record the compiled `.beam` bytes for `key`.
fn store_beam(key: CacheKey, beam: Vec<u8>) {
    cache_lock(compile_cache()).insert(key, beam);
}

/// The identity of a compiled NIF library: everything that determines its `.so` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NifCacheKey {
    cc: PathBuf,
    include: PathBuf,
    name: &'static str,
    c_source: &'static str,
}

/// The process-local `.so` cache: [`NifCacheKey`] -> compiled library bytes.
///
/// It only ever short-circuits a redundant `cc` spawn (several NIF cases share one library);
/// the artifact handed to a case is still written into that case's own temp dir, so the
/// cache cannot change what is observed.
fn nif_cache() -> &'static Mutex<HashMap<NifCacheKey, Vec<u8>>> {
    static CACHE: OnceLock<Mutex<HashMap<NifCacheKey, Vec<u8>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A copy of the cached library bytes for `key`, or `None` on a miss.
fn cached_shared_object(key: &NifCacheKey) -> Option<Vec<u8>> {
    cache_lock(nif_cache()).get(key).cloned()
}

/// Record the compiled library bytes for `key`.
fn store_shared_object(key: NifCacheKey, library: Vec<u8>) {
    cache_lock(nif_cache()).insert(key, library);
}

/// A unique temporary directory name for one compiled module.
fn unique_temp_dir(module: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "differential-module-{}-{}-{}",
        std::process::id(),
        n,
        module
    ))
}

/// The file names tried when scanning `$PATH` for `erlc`.
fn erlc_program_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["erlc.exe", "erlc.cmd"]
    }
    #[cfg(not(windows))]
    {
        &["erlc"]
    }
}

/// The file names tried when scanning `$PATH` for a C compiler.
///
/// Only the conventional `cc` driver is tried: the NIF fixtures are built with the Unix
/// `-shared -fPIC` recipe, so a platform where that spelling is absent skips the C-compiled
/// cases loudly (`MISSING_CC_REASON`) rather than guessing at a toolchain.
fn cc_program_names() -> &'static [&'static str] {
    &["cc"]
}

#[cfg(test)]
mod tests {
    use super::{
        build, build_with, compile, resolve_cc_from, resolve_erl_include, resolve_erlc,
        resolve_erlc_from, Build, NifFixture,
    };
    use crate::error::Error;
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// An executable scratch file (a `#!/bin/sh` stub), removed with its directory.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "differential-module-build-{}-{}-{:?}",
                std::process::id(),
                tag,
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch { dir }
        }

        fn executable(&self, name: &str, body: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::write(&path, body).expect("write script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path).expect("stat").permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms).expect("chmod");
            }
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn erlc_env_override_must_exist() {
        let err = resolve_erlc_from(Some(OsString::from("/nonexistent/erlc-differential")), None)
            .expect_err("a nonexistent $ERLC must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$ERLC"));
    }

    #[test]
    fn erlc_env_override_is_used_when_it_exists() {
        let scratch = Scratch::new("env");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let resolved = resolve_erlc_from(Some(erlc.clone().into_os_string()), None)
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved, erlc);
    }

    #[test]
    fn erlc_is_found_by_scanning_path() {
        let scratch = Scratch::new("path");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let other = scratch.dir.join("nothing-here");
        let path = std::env::join_paths([other, scratch.dir.clone()]).expect("join paths");
        let resolved = resolve_erlc_from(None, Some(path))
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved, erlc);
    }

    #[test]
    fn a_missing_erlc_is_ok_none() {
        let scratch = Scratch::new("absent");
        let path = std::env::join_paths([scratch.dir.clone()]).expect("join paths");
        assert_eq!(resolve_erlc_from(None, Some(path)).expect("resolve"), None);
    }

    #[test]
    fn a_failing_compiler_is_a_loud_error() {
        let scratch = Scratch::new("fail");
        let erlc = scratch.executable("erlc", "#!/bin/sh\necho 'boom' 1>&2\nexit 1\n");
        let err = compile(&erlc, "-module(x).\n", "x").expect_err("a compile failure is an error");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn a_compiler_that_writes_nothing_is_a_loud_error() {
        let scratch = Scratch::new("nooutput");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let err = compile(&erlc, "-module(x).\n", "x").expect_err("no .beam is an error");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("no"), "{err}");
    }

    /// The `erlc` spawn goes through the shared `ETXTBSY` retry, and that retry is bounded.
    ///
    /// Holding the stub compiler open for writing makes Linux refuse every `exec` with
    /// `ETXTBSY`, so `compile` must return the **loud spawn error** once the attempts are
    /// exhausted — not a bogus "no `.beam`" compile result, and not an endless retry.
    #[cfg(unix)]
    #[test]
    fn a_compiler_that_cannot_be_exec_yet_is_a_loud_error_not_a_hang() {
        let scratch = Scratch::new("etxtbsy");
        let erlc = scratch.executable("erlc", "#!/bin/sh\nexit 0\n");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&erlc)
            .expect("open the stub compiler for writing");

        // A permanently held writer must surface loudly, not hang or look like a compile
        // failure — the retry is bounded.
        let err = compile(&erlc, "-module(x).\n", "x")
            .expect_err("a write-open file cannot be exec'd");
        match err {
            Error::Io { ref source, .. } => {
                assert_eq!(source.raw_os_error(), Some(26), "{source:?}")
            }
            other => panic!("expected a loud spawn error, got {other:?}"),
        }
        assert!(err.to_string().contains("failed to execute erlc"), "{err}");

        drop(held);
    }

    #[test]
    fn a_program_that_fabricates_the_beam_is_accepted_and_cleaned_up() {
        // A stub compiler that just creates the expected output file: exercises the
        // success path (and the temp-dir cleanup on drop) without needing a real erlc.
        let scratch = Scratch::new("fake");
        let erlc = scratch.executable(
            "erlc",
            "#!/bin/sh\nout=\nsrc=\nwhile [ $# -gt 0 ]; do case $1 in -o) shift; out=$1;; *) src=$1;; esac; shift; done\n\
             base=$(basename \"$src\" .erl)\nprintf 'BEAM' > \"$out/$base.beam\"\n",
        );
        let compiled = compile(&erlc, "-module(x).\n", "x").expect("compile");
        assert!(compiled.beam().is_file(), "the .beam exists");
        assert_eq!(std::fs::read(compiled.beam()).expect("read beam"), b"BEAM");
        let dir = compiled.dir().to_path_buf();
        assert!(dir.is_dir());
        drop(compiled);
        assert!(!dir.exists(), "the temp dir is removed on drop");
    }

    /// An `erlc` stub that writes the expected `.beam` and records every invocation (one
    /// line per run) in `counter`.
    fn counting_compiler(scratch: &Scratch, counter: &std::path::Path) -> PathBuf {
        let body = format!(
            "#!/bin/sh\necho x >> '{counter}'\nout=\nsrc=\n\
             while [ $# -gt 0 ]; do case $1 in -o) shift; out=$1;; *) src=$1;; esac; shift; done\n\
             base=$(basename \"$src\" .erl)\nprintf 'BEAM' > \"$out/$base.beam\"\n",
            counter = counter.display()
        );
        scratch.executable("erlc", &body)
    }

    /// How many times the stub compiler above has run.
    fn invocations(counter: &std::path::Path) -> usize {
        std::fs::read_to_string(counter)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn a_repeated_compile_reuses_the_cached_beam_without_reinvoking_erlc() {
        let scratch = Scratch::new("cache");
        let counter = scratch.dir.join("invocations");
        let erlc = counting_compiler(&scratch, &counter);
        let source = "-module(cached_probe).\n";

        let first = compile(&erlc, source, "cached_probe").expect("first compile");
        assert_eq!(invocations(&counter), 1, "the first compile runs erlc");
        let first_dir = first.dir().to_path_buf();
        let first_beam = std::fs::read(first.beam()).expect("read the beam");

        // The same (erlc, module, source) triple is a cache hit: no second erlc process,
        // but the case still gets its own freshly materialized directory.
        let second = compile(&erlc, source, "cached_probe").expect("second compile");
        assert_eq!(
            invocations(&counter),
            1,
            "a repeated compile must not spawn erlc again"
        );
        assert_eq!(
            std::fs::read(second.beam()).expect("read the beam"),
            first_beam,
            "the cached beam bytes are identical"
        );
        assert_ne!(first_dir, second.dir(), "each case keeps its own temp dir");
        assert!(
            second.dir().join("cached_probe.erl").is_file(),
            "a materialized dir mirrors an erlc-built one (source + beam)"
        );

        // The key covers the source: a different source is a genuine cache miss.
        compile(&erlc, "-module(cached_probe).\n%% v2\n", "cached_probe").expect("third compile");
        assert_eq!(invocations(&counter), 2, "a different source recompiles");

        // Every materialized dir is cleaned up on drop, exactly like an erlc-built one.
        let second_dir = second.dir().to_path_buf();
        drop(first);
        drop(second);
        assert!(!first_dir.exists(), "the first temp dir is removed on drop");
        assert!(
            !second_dir.exists(),
            "the cached temp dir is removed on drop"
        );
    }

    #[test]
    fn concurrent_compiles_of_the_same_source_all_yield_a_usable_beam() {
        // The cache is shared, so it must be safe to reach from several threads at once:
        // every caller gets a valid, private temp dir and erlc never runs more than once
        // per thread.
        let scratch = Scratch::new("cache-threads");
        let counter = scratch.dir.join("invocations");
        let erlc = counting_compiler(&scratch, &counter);
        let source = "-module(cached_threads).\n";

        let results: Vec<_> = std::thread::scope(|scope| {
            (0..4)
                .map(|_| {
                    let erlc = erlc.clone();
                    let counter = counter.clone();
                    scope.spawn(move || {
                        let compiled = compile(&erlc, source, "cached_threads").expect("compile");
                        assert!(compiled.beam().is_file());
                        assert_eq!(std::fs::read(compiled.beam()).expect("read beam"), b"BEAM");
                        drop(compiled);
                        invocations(&counter)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect()
        });

        assert!(
            results.iter().all(|&n| (1usize..=4).contains(&n)),
            "erlc ran once per thread at most: {results:?}"
        );
    }

    #[test]
    fn resolve_erlc_reads_the_real_environment_without_panicking() {
        let _ = resolve_erlc();
    }

    // --- the C toolchain resolvers (contract A.3) ----------------------------------------

    #[test]
    fn cc_env_override_must_be_usable() {
        let scratch = Scratch::new("cc-env-bad");
        let path = std::env::join_paths([scratch.dir.clone()]).expect("join paths");
        let err = resolve_cc_from(
            Some(OsString::from("/nonexistent/cc-differential")),
            Some(path),
        )
        .expect_err("a nonexistent $CC must not resolve");
        assert!(matches!(err, Error::Env(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("$CC"));
    }

    #[test]
    fn cc_env_override_is_used_when_it_exists() {
        let scratch = Scratch::new("cc-env");
        let cc = scratch.executable("my-cc", "#!/bin/sh\nexit 0\n");
        let resolved = resolve_cc_from(Some(cc.clone().into_os_string()), None)
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved, cc);
    }

    #[test]
    fn cc_env_override_may_name_a_command_on_path() {
        // `CC=gcc` is the conventional spelling, so a bare name is looked up on `$PATH`
        // before the override is rejected. A unique name keeps the test independent of the
        // process's own working directory (a relative path is also a candidate).
        let scratch = Scratch::new("cc-name");
        let cc = scratch.executable("cc-fixture-probe", "#!/bin/sh\nexit 0\n");
        let path = std::env::join_paths([scratch.dir.clone()]).expect("join paths");
        let resolved = resolve_cc_from(Some(OsString::from("cc-fixture-probe")), Some(path))
            .expect("resolve")
            .expect("present");
        assert_eq!(resolved, cc);
    }

    #[test]
    fn cc_is_found_by_scanning_path_and_absence_is_ok_none() {
        let scratch = Scratch::new("cc-path");
        let cc = scratch.executable("cc", "#!/bin/sh\nexit 0\n");
        let path = std::env::join_paths([scratch.dir.clone()]).expect("join paths");
        assert_eq!(
            resolve_cc_from(None, Some(path)).expect("resolve"),
            Some(cc)
        );

        // A `$PATH` with no compiler (and no `$CC`) is `Ok(None)`: a skip, not an error.
        let empty = std::env::join_paths([scratch.dir.join("nothing-here")]).expect("join paths");
        assert_eq!(resolve_cc_from(None, Some(empty)).expect("resolve"), None);
    }

    #[test]
    fn the_erl_nif_include_probe_is_consistent_and_really_holds_the_header() {
        // The probe is cached process-locally, so repeated calls must agree; and whenever it
        // answers, the answer must actually hold `erl_nif.h` (a wrong directory would make
        // every C fixture fail to compile).
        let first = resolve_erl_include();
        assert_eq!(first, resolve_erl_include(), "the probe is cached");
        if let Some(dir) = first {
            assert!(
                dir.join("erl_nif.h").is_file(),
                "the located include dir {} holds no erl_nif.h",
                dir.display()
            );
        } else {
            println!("note: no erl_nif.h include directory found (the NIF cases skip)");
        }
    }

    // --- the fixture build (contract A.3) ------------------------------------------------

    /// A stub compiler that writes the expected `.beam`, so `build_with` can be driven
    /// without a real `erlc`.
    fn fabricating_compiler(scratch: &Scratch) -> PathBuf {
        scratch.executable(
            "erlc",
            "#!/bin/sh\nout=\nsrc=\nwhile [ $# -gt 0 ]; do case $1 in -o) shift; out=$1;; *) src=$1;; esac; shift; done\n\
             base=$(basename \"$src\" .erl)\nprintf 'BEAM' > \"$out/$base.beam\"\n",
        )
    }

    #[test]
    fn a_missing_erlc_skips_without_building_anything() {
        let outcome = build_with(None, None, None, "-module(x).\n", "x", None).expect("build");
        match outcome {
            Build::Skipped(reason) => assert!(reason.contains("erlc"), "{reason}"),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_c_compiler_skips_a_compiled_fixture_before_running_erlc() {
        let scratch = Scratch::new("skip-cc");
        let counter = scratch.dir.join("invocations");
        let erlc = counting_compiler(&scratch, &counter);
        let outcome = build_with(
            Some(erlc),
            None,
            Some(PathBuf::from("/usr/include")),
            "-module(x).\n",
            "x",
            Some(NifFixture::CompiledLibrary {
                name: "x",
                c_source: "int x(void) { return 0; }\n",
            }),
        )
        .expect("build");
        match outcome {
            Build::Skipped(reason) => assert!(reason.contains("C compiler"), "{reason}"),
            other => panic!("expected a skip, got {other:?}"),
        }
        assert_eq!(
            invocations(&counter),
            0,
            "a skipped case must not have run erlc"
        );
    }

    #[test]
    fn a_missing_erl_nif_header_skips_a_compiled_fixture() {
        let scratch = Scratch::new("skip-header");
        let erlc = fabricating_compiler(&scratch);
        let outcome = build_with(
            Some(erlc),
            Some(PathBuf::from("/usr/bin/cc")),
            None,
            "-module(x).\n",
            "x",
            Some(NifFixture::CompiledLibrary {
                name: "x",
                c_source: "int x(void) { return 0; }\n",
            }),
        )
        .expect("build");
        match outcome {
            Build::Skipped(reason) => assert!(reason.contains("erl_nif.h"), "{reason}"),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn a_text_fixture_is_written_verbatim_and_needs_no_c_toolchain() {
        // The text `.so` case is what makes the `load_failed` shape reachable on a machine
        // with no C compiler at all: it must not consult `cc` or `erl_nif.h`.
        let scratch = Scratch::new("text-fixture");
        let erlc = fabricating_compiler(&scratch);
        let outcome = build_with(
            Some(erlc),
            None,
            None,
            "-module(nif_probe).\n",
            "nif_probe",
            Some(NifFixture::TextFileSo {
                name: "nif_probe_text",
                contents: "not a shared object\n",
            }),
        )
        .expect("build");
        let Build::Compiled(compiled) = outcome else {
            panic!("expected a compiled module, got {outcome:?}");
        };
        let so = compiled.dir().join("nif_probe_text.so");
        assert!(so.is_file(), "the text fixture must be materialized");
        assert_eq!(
            std::fs::read_to_string(&so).expect("read the text .so"),
            "not a shared object\n"
        );
        assert!(
            compiled.beam().is_file(),
            "the module is still compiled next to it"
        );
    }

    /// The real thing: a compiled module plus a real NIF library next to it. Skips **loudly**
    /// (never silently) when the host facilities are missing.
    #[cfg(unix)]
    #[test]
    fn a_compiled_library_fixture_lands_next_to_the_beam() {
        let c_source = "int not_a_nif(void) { return 0; }\n";
        match build(
            "-module(nif_probe).\n-export([f/0]).\nf() -> ok.\n",
            "nif_probe",
            Some(NifFixture::CompiledLibrary {
                name: "nif_probe",
                c_source,
            }),
        )
        .expect("build")
        {
            Build::Compiled(compiled) => {
                let so = compiled.dir().join("nif_probe.so");
                assert!(so.is_file(), "the library must be materialized as {}", so.display());
                assert!(compiled.beam().is_file());
                // A second build of the same fixture is served from the byte cache and still
                // lands in its own temp dir.
                let Build::Compiled(second) = build(
                    "-module(nif_probe).\n-export([f/0]).\nf() -> ok.\n",
                    "nif_probe",
                    Some(NifFixture::CompiledLibrary {
                        name: "nif_probe",
                        c_source,
                    }),
                )
                .expect("build")
                else {
                    panic!("the second build must also compile");
                };
                assert_ne!(compiled.dir(), second.dir());
                assert_eq!(
                    std::fs::read(second.dir().join("nif_probe.so")).expect("read the cached .so"),
                    std::fs::read(&so).expect("read the .so"),
                    "a cache hit writes the very same bytes"
                );
            }
            Build::Skipped(reason) => println!("SKIP nif fixture build: {reason}"),
        }
    }

    #[test]
    fn a_failing_c_compiler_is_a_loud_error_not_a_skip() {
        // A *present* compiler that fails is a genuine tool failure: the case must not be
        // reported as a capability gap.
        let scratch = Scratch::new("cc-fails");
        let erlc = fabricating_compiler(&scratch);
        let cc = scratch.executable("cc", "#!/bin/sh\necho 'cc exploded' 1>&2\nexit 1\n");
        let err = build_with(
            Some(erlc),
            Some(cc),
            Some(scratch.dir.clone()),
            "-module(nif_probe).\n",
            "nif_probe",
            Some(NifFixture::CompiledLibrary {
                name: "nif_probe",
                c_source: "int not_a_nif(void) { return 0; }\n",
            }),
        )
        .expect_err("a failing compiler is a loud error");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("cc exploded"), "{err}");
    }

    #[test]
    fn a_c_compiler_that_writes_nothing_is_a_loud_error() {
        let scratch = Scratch::new("cc-nooutput");
        let erlc = fabricating_compiler(&scratch);
        let cc = scratch.executable("cc", "#!/bin/sh\nexit 0\n");
        let err = build_with(
            Some(erlc),
            Some(cc),
            Some(scratch.dir.clone()),
            "-module(nif_probe).\n",
            "nif_probe",
            Some(NifFixture::CompiledLibrary {
                name: "nif_probe",
                c_source: "int not_a_nif(void) { return 0; }\n",
            }),
        )
        .expect_err("no .so is a loud error");
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("produced no"), "{err}");
    }
}
