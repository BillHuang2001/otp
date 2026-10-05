//! The **code server**: one shared [`AtomTable`], a module cache and an ordered code
//! path.
//!
//! A real BEAM has exactly **two** global resources that every loaded module shares:
//! a single atom table and a single code area. `beam_rt` keeps them here.
//!
//! # Why the shared atom table matters
//!
//! The loader's convenience entry points ([`beam_loader::load`]/[`load_file`]) build a
//! **private** [`AtomTable`] per module, with indices assigned in that module's
//! `AtU8` wire order. That is exactly right for a single-module run, but wrong for a
//! multi-module host: the same name (`true`, `ok`, …) would get a different index in
//! every module, the engine compares atom *names* through the running module's table,
//! and a BIF that builds an atom would produce an index the caller's module could not
//! name. So this server re-runs the loader's **public** pipeline
//! ([`container`](beam_loader::container) → [`chunk`](beam_loader::chunk) →
//! [`decode`](beam_loader::decode) → [`lower::build_module`](beam_loader::lower::build_module))
//! seeded with **one** shared table. Interning is idempotent and name-keyed, so every
//! module sees the same index for a given name.
//!
//! [`AtomTable`] indices are assigned in insertion order, so a module loaded **later**
//! keeps the shared prefix it was loaded with; a term produced by that module is
//! nameable by the server's (now larger) table. Nothing ever renumbers.
//!
//! # The code path
//!
//! [`CodeServer::find_file`] searches, in order: the `-pa` directories, the `-pz`
//! directories (both as configured) and finally the **system** `ebin` directories
//! (stdlib, kernel) resolved from the running Erlang installation at start-up.
//!
//! # Failure is loud
//!
//! A module that cannot be found is a typed error naming every directory that was
//! searched — never a silent miss. The system code path is resolved either from the
//! `ERTS_STDLIB_DIR` environment override or by asking the system `erl`; if neither
//! works, [`CodeServer::add_system_paths`] reports an honest error rather than
//! quietly running without a standard library.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use beam_common::Result;
use beam_loader::{chunk, container, decode, lower, Atom, Module};
use beam_term::AtomTable;

use crate::error;

/// The **code server**: the runtime's one shared atom table, its module cache and its
/// ordered code path.
pub struct CodeServer {
    /// The one atom table every loaded module is interned into.
    atoms: AtomTable,
    /// Loaded modules, by name — `Arc` so a caller can hold one while the server keeps
    /// loading others (`Module` is `!Send`/`!Sync`, so it is never shared across threads).
    modules: HashMap<String, Arc<Module>>,
    /// The code path: `-pa` dirs, then `-pz` dirs, then the system `ebin` dirs.
    paths: Vec<PathBuf>,
}

impl CodeServer {
    /// An empty server: no atoms, no modules, no code path.
    ///
    /// The system `ebin` directories are added explicitly with
    /// [`CodeServer::add_system_paths`], so a caller that only needs an explicit code
    /// path never depends on a system Erlang installation.
    pub fn new() -> CodeServer {
        CodeServer {
            atoms: AtomTable::new(),
            modules: HashMap::new(),
            paths: Vec::new(),
        }
    }

    /// Appends `dir` to the code path (the `-pa`/`-pz` directories, in order).
    ///
    /// A directory already present is not added twice.
    pub fn add_path(&mut self, dir: impl Into<PathBuf>) {
        let dir = dir.into();
        if !self.paths.contains(&dir) {
            self.paths.push(dir);
        }
    }

    /// Appends the **system** `ebin` directories (stdlib, then kernel) to the code path.
    ///
    /// The directories are resolved once per process (see [`system_ebins`]) from
    /// `$ERTS_STDLIB_DIR` or, failing that, from the system `erl`. A failure here is a
    /// hard error: running without a standard library is never silent.
    ///
    /// # Errors
    ///
    /// A message-carrying [`beam_common::BeamError`] naming the environment variable
    /// or the `erl` invocation that failed.
    pub fn add_system_paths(&mut self) -> Result<()> {
        for dir in system_ebins()? {
            self.add_path(dir);
        }
        Ok(())
    }

    /// The one shared atom table.
    pub fn atoms(&self) -> &AtomTable {
        &self.atoms
    }

    /// The one shared atom table, mutably — the runtime seeds and interns through it.
    pub fn atoms_mut(&mut self) -> &mut AtomTable {
        &mut self.atoms
    }

    /// The code path, in search order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// The path of `<name>.beam`, searched in code-path order, if it exists.
    pub fn find_file(&self, name: &str) -> Option<PathBuf> {
        let file = format!("{name}.beam");
        self.paths
            .iter()
            .map(|dir| dir.join(&file))
            .find(|candidate| candidate.is_file())
    }

    /// Whether `name` has been loaded (and cached) already.
    pub fn is_loaded(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    /// The names of the cached modules, sorted.
    pub fn loaded(&self) -> Vec<String> {
        let mut names: Vec<String> = self.modules.keys().cloned().collect();
        names.sort();
        names
    }

    /// A loaded module by name.
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules.get(name).map(Arc::as_ref)
    }

    /// Every loaded module (for fun resolution).
    pub fn modules(&self) -> impl Iterator<Item = &Arc<Module>> {
        self.modules.values()
    }

    /// Loads `name` (memoized): the cached module, or `<name>.beam` from the code path.
    ///
    /// # Errors
    ///
    /// A message-carrying error naming every directory searched when `<name>.beam` is
    /// not found, plus the loader's own error for a malformed `.beam`.
    pub fn load(&mut self, name: &str) -> Result<Arc<Module>> {
        if let Some(module) = self.modules.get(name) {
            return Ok(module.clone());
        }
        let path = self
            .find_file(name)
            .ok_or_else(|| self.not_found(name))?;
        self.load_path(&path)
    }

    /// Loads the `.beam` at `path` and caches it under the module's **own** name.
    ///
    /// This is the explicit-file entry point (the CLI's `run` action and the slice
    /// fixture): the file need not be on the code path.
    ///
    /// # Errors
    ///
    /// A read error for `path`, the loader's error for a malformed image, or a
    /// diagnostic when the image has no resolvable module name.
    pub fn load_path(&mut self, path: impl AsRef<Path>) -> Result<Arc<Module>> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| {
            error::message(format!("cannot read {}: {source}", path.display()))
        })?;
        let module = self.load_bytes(&bytes)?;
        let name = module
            .atom_name(module.name)
            .ok_or_else(|| error::message(format!("{} has no module name", path.display())))?
            .to_string();
        let module = Arc::new(module);
        self.modules.insert(name, module.clone());
        Ok(module)
    }

    /// Loads a `.beam` image out of memory into the shared atom table.
    ///
    /// This is [`beam_loader::load`]'s exact pipeline re-run through the loader's
    /// public modules, with `self.atoms` as the interning context (see the module docs).
    ///
    /// # Errors
    ///
    /// The loader's error (a malformed container/chunk/instruction stream).
    pub fn load_bytes(&mut self, bytes: &[u8]) -> Result<Module> {
        let container = container::parse(bytes)?;
        let chunks = chunk::parse(&container)?;

        // `atoms[0]` is the NIL placeholder; `atoms[1]` is the module name (mirrors
        // `load`). Interning is idempotent, so a name shared with an earlier module
        // keeps that module's index.
        let mut wire: Vec<Atom> = Vec::with_capacity(chunks.atoms.len());
        wire.push(Atom::MAX);
        for name in &chunks.atoms[1..] {
            wire.push(self.atoms.intern(name));
        }

        let raw = decode::decode_stream(
            chunks.code_stream,
            &wire,
            &chunks.code_version,
            &chunks.types,
        )?;

        // `build_module` owns (and may extend) a clone of the shared table while it
        // decodes the `LitT` literals; any atom that appeared only in a literal is
        // appended after the clone's prefix at exactly `atoms.len()..`, so merging
        // them back in index order keeps the shared indices identical.
        let module = lower::build_module(&chunks, &raw, self.atoms.clone())?;
        let base = self.atoms.len();
        for index in base..module.atom_table.len() {
            if let Some(name) = module.atom_table.name(Atom::from_raw(index as u32)) {
                self.atoms.intern(name);
            }
        }

        Ok(module)
    }

    /// The error returned when `<name>.beam` is on no code-path directory.
    fn not_found(&self, name: &str) -> beam_common::BeamError {
        if self.paths.is_empty() {
            error::message(format!(
                "cannot find {name}.beam: the code path is empty (no -pa/-pz directory and no system stdlib)"
            ))
        } else {
            let searched: Vec<String> = self.paths.iter().map(|p| p.display().to_string()).collect();
            error::message(format!(
                "cannot find {name}.beam in the code path [{}]",
                searched.join(", ")
            ))
        }
    }
}

impl Default for CodeServer {
    /// Same as [`CodeServer::new`].
    fn default() -> CodeServer {
        CodeServer::new()
    }
}

// ---------------------------------------------------------------------------
// Resolving the system code path (once per process)
// ---------------------------------------------------------------------------

/// The system `ebin` directories (stdlib, kernel), resolved **once** per process.
///
/// # Errors
///
/// The recorded resolution failure (an unusable `$ERTS_STDLIB_DIR` or a failed `erl`
/// invocation), reported loyally on every call.
fn system_ebins() -> Result<Vec<PathBuf>> {
    static CACHE: OnceLock<std::result::Result<Vec<PathBuf>, String>> = OnceLock::new();
    match CACHE.get_or_init(|| resolve_system_ebins().map_err(|error| error.to_string())) {
        Ok(dirs) => Ok(dirs.clone()),
        Err(message) => Err(error::message(message.clone())),
    }
}

/// Resolves the system `ebin` directories from the environment or the system `erl`.
fn resolve_system_ebins() -> Result<Vec<PathBuf>> {
    if let Ok(dir) = env::var("ERTS_STDLIB_DIR") {
        let dir = PathBuf::from(dir);
        let ebin = app_ebin(&dir, "lists").ok_or_else(|| {
            error::message(format!(
                "$ERTS_STDLIB_DIR ({}) does not contain lists.beam (neither directly nor under ebin/)",
                dir.display()
            ))
        })?;
        let mut dirs = vec![ebin.clone()];
        if let Some(kernel) = lib_root(&ebin).and_then(|root| find_app_ebin(&root, "kernel")) {
            dirs.push(kernel);
        }
        return Ok(dirs);
    }

    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s~n~s~n\",[code:lib_dir(stdlib),code:lib_dir(kernel)]),halt().",
        ])
        .output()
        .map_err(|source| {
            error::message(format!(
                "cannot run `erl` to locate the system code path (set $ERTS_STDLIB_DIR to override): {source}"
            ))
        })?;
    if !output.status.success() {
        return Err(error::message(format!(
            "`erl` exited with {} while reporting the system code path",
            output.status
        )));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| {
        error::message("`erl` reported a non-UTF-8 system code path")
    })?;
    let app_dirs: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if app_dirs.is_empty() {
        return Err(error::message(
            "`erl` did not report a system code path (is a standard Erlang installed?)",
        ));
    }

    let mut dirs = Vec::new();
    for (index, app_dir) in app_dirs.iter().enumerate() {
        let app_dir = PathBuf::from(app_dir);
        match index {
            // stdlib must be usable, or nothing below it can be loaded.
            0 => {
                let ebin = app_ebin(&app_dir, "lists").ok_or_else(|| {
                    error::message(format!(
                        "the system stdlib ({}) does not contain lists.beam",
                        app_dir.display()
                    ))
                })?;
                dirs.push(ebin);
            }
            // kernel is best-effort: `<app dir>/ebin` when it holds kernel.beam.
            _ => {
                if let Some(ebin) = app_ebin(&app_dir, "kernel") {
                    dirs.push(ebin);
                }
            }
        }
    }
    Ok(dirs)
}

/// The `ebin` directory holding `<module>.beam`, given either the application
/// directory or its `ebin` subdirectory.
fn app_ebin(dir: &Path, module: &str) -> Option<PathBuf> {
    let file = format!("{module}.beam");
    if dir.join(&file).is_file() {
        return Some(dir.to_path_buf());
    }
    let ebin = dir.join("ebin");
    if ebin.join(&file).is_file() {
        return Some(ebin);
    }
    None
}

/// The `<lib>` directory an `…/<lib>/<app>-<vsn>/ebin` path belongs to.
fn lib_root(ebin: &Path) -> Option<PathBuf> {
    // <lib>/<app>-<vsn>/ebin -> <lib>/<app>-<vsn> -> <lib>
    ebin.parent()?.parent().map(Path::to_path_buf)
}

/// The `ebin` directory of the `<app>-<vsn>` sibling of `lib_root`, if present.
fn find_app_ebin(lib_root: &Path, app: &str) -> Option<PathBuf> {
    let prefix = format!("{app}-");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(lib_root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect();
    candidates.sort();
    candidates
        .iter()
        .find_map(|candidate| app_ebin(candidate, app))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server over the system code path, or `None` when no system Erlang is
    /// available (the caller then skips **loudly**, never silently passing).
    fn system_server() -> Option<CodeServer> {
        let mut server = CodeServer::new();
        match server.add_system_paths() {
            Ok(()) => Some(server),
            Err(error) => {
                eprintln!("SKIP: no system code path ({error})");
                None
            }
        }
    }

    #[test]
    fn an_empty_server_has_no_atoms_no_modules_and_no_path() {
        let mut server = CodeServer::new();
        assert!(server.atoms().is_empty());
        assert!(server.paths().is_empty());
        assert!(server.loaded().is_empty());
        assert!(!server.is_loaded("lists"));
        assert_eq!(server.find_file("lists"), None);
        let error = server
            .load("lists")
            .expect_err("an empty code path cannot resolve a module");
        assert!(error.to_string().contains("lists.beam"), "{error}");
        assert!(CodeServer::default().paths().is_empty());
    }

    #[test]
    fn add_path_is_ordered_and_deduplicated() {
        let mut server = CodeServer::new();
        server.add_path("/a");
        server.add_path("/b");
        server.add_path("/a");
        assert_eq!(
            server.paths(),
            &[PathBuf::from("/a"), PathBuf::from("/b")][..]
        );
    }

    #[test]
    fn a_missing_module_names_every_searched_directory() {
        let mut server = CodeServer::new();
        server.add_path("/one");
        server.add_path("/two");
        let error = server.load("nosuchmodule").expect_err("not on the code path");
        let message = error.to_string();
        assert!(message.contains("nosuchmodule.beam"), "{message}");
        assert!(message.contains("/one"), "{message}");
        assert!(message.contains("/two"), "{message}");
    }

    #[test]
    fn loads_the_system_lists_module_into_the_shared_table() {
        let Some(mut server) = system_server() else {
            return;
        };
        let base = server.atoms().len();
        let lists = server.load("lists").expect("stdlib lists loads");
        let name = lists.atom_name(lists.name).expect("the module has a name");
        assert_eq!(name, "lists");
        // The load interned the module's atoms into the *shared* table, after its prefix.
        assert!(server.atoms().len() > base);
        assert_eq!(lists.atom_table.lookup("reverse"), server.atoms().lookup("reverse"));
        // The module's own name resolves through `find_file` and the cache.
        assert!(server.is_loaded("lists"));
        assert_eq!(server.loaded(), vec!["lists".to_string()]);
        assert!(server.find_file("lists").is_some());
    }

    #[test]
    fn two_modules_share_one_atom_universe() {
        let Some(mut server) = system_server() else {
            return;
        };
        let lists = server.load("lists").expect("lists loads");
        let proplists = server.load("proplists").expect("proplists loads");
        // Both modules were interned into the same table, so a name shared with the
        // first module keeps the *same* index in the second: this is what makes a
        // cross-module atom comparison work.
        for name in ["true", "false", "ok", "error"] {
            if let (Some(a), Some(b)) = (lists.atom_table.lookup(name), proplists.atom_table.lookup(name))
            {
                assert_eq!(a, b, "the shared atom `{name}` must not be renumbered");
            }
        }
        assert_eq!(server.loaded(), vec!["lists".to_string(), "proplists".to_string()]);
    }

    #[test]
    fn loading_is_idempotent_and_cached() {
        let Some(mut server) = system_server() else {
            return;
        };
        let first = server.load("lists").expect("lists loads");
        let second = server.load("lists").expect("cached");
        assert!(Arc::ptr_eq(&first, &second), "the cache returns the same module");
        drop(first);
        drop(second);
        assert_eq!(server.loaded().len(), 1);
    }
}
