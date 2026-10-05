//! The runtime configuration: the parsed emulator switches plus the selected action.
//!
//! [`RuntimeConfig`] wraps the `beam_common` A.6 **emulator skeleton** ([`EmulatorConfig`])
//! with the things only `beam_rt` owns: the **code path** (`-pa`/`-pz`), the parsed
//! **init arguments** (contract A.6 — the flag surface real `erl` hands to
//! `init:get_argument/1`) and the **action** the CLI was asked to perform
//! (`-version`, `-eval`, `-s`, `-run`, `run`, …). The parser that fills it in lives in
//! [`crate::cli`].

use std::path::PathBuf;

use crate::config::Action::{Boot, Version};

/// The emulator start-up surface — `beam_common`'s A.6 configuration skeleton.
///
/// `beam_rt` re-exports the type under this name because from the runtime's point of
/// view it *is* the emulator configuration; the full `+`/`-`/`-M*` parser lives here.
pub type EmulatorConfig = beam_common::RuntimeConfig;

/// The default process-heap size in term words (64 Ki words — ample for the
/// Milestone-1 vertical slice; there is no collector yet, see `runtime.rs`).
pub const DEFAULT_HEAP_WORDS: usize = 65_536;

/// The parsed **init arguments**: for every command-line flag, the list of its
/// occurrences, each occurrence being the list of values that followed it.
///
/// This is the shape `init:get_argument/1` reports (contract A.6):
///
/// * `beam_rt -noshell` records `noshell -> [[]]`, so
///   `init:get_argument(noshell)` is `{ok,[[]]}` (a valueless flag);
/// * `beam_rt -pa /a -pa /b` records `pa -> [["/a"], ["/b"]]`, so
///   `init:get_argument(pa)` is `{ok,[["/a"],["/b"]]}`;
/// * a flag that was never given has **no** entry, so `init:get_argument(f)` is the
///   atom `error`.
///
/// Every flag goes in — including the ones this runtime models itself — because a
/// program is entitled to observe its own command line. A flag's name is its switch
/// with the leading `-`/`+` run removed (`--nope` → `nope`, `+Weird` → `Weird`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitArgs {
    occurrences: Vec<(String, Vec<String>)>,
}

impl InitArgs {
    /// No init arguments.
    pub fn new() -> InitArgs {
        InitArgs::default()
    }

    /// Records one occurrence of `flag` with `values`.
    pub fn record(&mut self, flag: &str, values: Vec<String>) {
        self.occurrences.push((flag.to_string(), values));
    }

    /// Every occurrence of `flag`, each as its list of values — `None` when the flag
    /// was never given (which `init:get_argument/1` reports as `error`).
    pub fn get(&self, flag: &str) -> Option<Vec<&[String]>> {
        let found: Vec<&[String]> = self
            .occurrences
            .iter()
            .filter(|(name, _)| name == flag)
            .map(|(_, values)| values.as_slice())
            .collect();
        if found.is_empty() {
            None
        } else {
            Some(found)
        }
    }

    /// The number of recorded flag occurrences.
    pub fn len(&self) -> usize {
        self.occurrences.len()
    }

    /// Whether no flag was recorded.
    pub fn is_empty(&self) -> bool {
        self.occurrences.is_empty()
    }

    /// Every recorded `(flag, values)` occurrence, in command-line order.
    pub fn occurrences(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.occurrences
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice()))
    }
}

/// One `run FILE MOD:FUNC/ARITY ARGS…` argument, before it is materialized on the
/// runtime heap.
///
/// The grammar (documented on [`crate::cli::parse`]) has four forms: a **small
/// integer** (optionally negative), a **bare atom** (`a`, `abc`, `foo_bar`), a
/// **string** (`"text"`, an Erlang char list) and a **list** of arguments (`[1,a]`).
/// They are parsed and validated by the CLI, and materialized once a [`crate::Runtime`]
/// (whose shared atom table and heap they need) exists — an atom's *index* can only be
/// assigned by the runtime's one atom table, so it cannot be built while parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgTerm {
    /// A small integer literal (may be negative).
    Int(i64),
    /// A bare atom name, interned in the runtime's shared atom table when materialized.
    Atom(String),
    /// A double-quoted string, materialized as an Erlang char list.
    Str(String),
    /// A `[...]` list of arguments, materialized as a proper list.
    List(Vec<ArgTerm>),
}

/// What the runtime was asked to do.
///
/// Exactly one action is selected by the command line; without an action switch the
/// runtime boots and exits ([`Action::Boot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Boot the runtime and exit (`-noshell` with no action switch).
    Boot,
    /// Print the version banner and exit (`-version`/`--version`).
    Version,
    /// Evaluate the documented integer-expression subset and **discard** the result
    /// (`-eval Expr`). Real `erl -eval '2+3'` prints nothing and exits 0.
    Eval(String),
    /// Apply `module:function(AtomArgs…)` and discard the result (`-s Mod Func …`).
    ///
    /// The arguments are the **atom names** given on the command line, exactly as
    /// real `erl` treats them.
    Apply {
        /// The module to call into.
        module: String,
        /// The function to call.
        function: String,
        /// The atom arguments, verbatim from the command line.
        args: Vec<String>,
    },
    /// Apply `module:function(StringArgs…)` and discard the result
    /// (`-run Mod Func …`).
    ///
    /// Unlike `-s`, every argument is a **string** (an Erlang char list) — real
    /// `erl` semantics.
    Run {
        /// The module to call into.
        module: String,
        /// The function to call.
        function: String,
        /// The string arguments, verbatim from the command line.
        args: Vec<String>,
    },
    /// Load a `.beam` file, run `module:function/arity` and print the result
    /// (`run <file.beam> <mod>:<func>/<arity> [args…]`).
    RunFile {
        /// The `.beam` file to load.
        file: PathBuf,
        /// The module the file is expected to define.
        module: String,
        /// The function to run.
        function: String,
        /// The arity to run.
        arity: u32,
        /// The parsed term arguments (materialized on the runtime heap when run).
        args: Vec<ArgTerm>,
    },
}

/// The complete configuration of one `beam_rt` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// The emulator start-up surface of this invocation (`beam_common`'s A.6 skeleton).
    pub emulator: EmulatorConfig,
    /// The code path: `-pa` directories first, then `-pz` directories, in order.
    pub code_paths: Vec<PathBuf>,
    /// The process-heap size in term words.
    pub heap_words: usize,
    /// The parsed init arguments (contract A.6; the source of `init:get_argument/1`).
    pub init_args: InitArgs,
    /// The action to perform.
    pub action: Action,
}

impl Default for RuntimeConfig {
    /// The defaults of a bare `beam_rt` invocation: a `jit` emulator, an empty code
    /// path, a [`DEFAULT_HEAP_WORDS`]-word heap, no init arguments and [`Action::Boot`].
    fn default() -> Self {
        RuntimeConfig {
            emulator: EmulatorConfig::default(),
            code_paths: Vec::new(),
            heap_words: DEFAULT_HEAP_WORDS,
            init_args: InitArgs::default(),
            action: Boot,
        }
    }
}

impl RuntimeConfig {
    /// The default configuration (same as [`Default::default`]).
    pub fn new() -> Self {
        RuntimeConfig::default()
    }

    /// Sets the action to perform.
    pub fn with_action(mut self, action: Action) -> Self {
        self.action = action;
        self
    }

    /// Replaces the code path with `paths`.
    pub fn with_code_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.code_paths = paths;
        self
    }

    /// Appends `path` to the code path.
    pub fn with_code_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.code_paths.push(path.into());
        self
    }

    /// Sets the process-heap size in term words.
    pub fn with_heap_words(mut self, heap_words: usize) -> Self {
        self.heap_words = heap_words;
        self
    }

    /// Replaces the parsed init arguments.
    pub fn with_init_args(mut self, init_args: InitArgs) -> Self {
        self.init_args = init_args;
        self
    }

    /// Replaces the emulator start-up surface.
    pub fn with_emulator(mut self, emulator: EmulatorConfig) -> Self {
        self.emulator = emulator;
        self
    }

    /// Sets the number of normal schedulers (`+S`).
    pub fn with_schedulers(mut self, schedulers: u32) -> Self {
        self.emulator.schedulers = Some(schedulers);
        self
    }

    /// Sets the maximum number of processes (`+P`).
    pub fn with_max_processes(mut self, max_processes: usize) -> Self {
        self.emulator.max_processes = Some(max_processes);
        self
    }

    /// Sets the distribution node name (`-name`/`-sname`).
    pub fn with_node_name(mut self, node_name: impl Into<String>) -> Self {
        self.emulator.node_name = Some(node_name.into());
        self
    }

    /// Whether the configured action is [`Action::Version`].
    pub fn is_version(&self) -> bool {
        self.action == Version
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn defaults() {
        let config = RuntimeConfig::default();
        assert_eq!(config, RuntimeConfig::new());
        assert_eq!(config.emulator, EmulatorConfig::default());
        assert!(config.code_paths.is_empty());
        assert_eq!(config.heap_words, DEFAULT_HEAP_WORDS);
        assert!(config.init_args.is_empty());
        assert_eq!(config.action, Action::Boot);
        assert!(!config.is_version());
    }

    #[test]
    fn builders() {
        let config = RuntimeConfig::new()
            .with_action(Action::Version)
            .with_code_path("/code/a")
            .with_code_paths(vec![PathBuf::from("/code/b")])
            .with_heap_words(1024)
            .with_schedulers(2)
            .with_max_processes(1000)
            .with_node_name("beam@host");
        assert_eq!(config.action, Action::Version);
        assert!(config.is_version());
        assert_eq!(config.code_paths, vec![PathBuf::from("/code/b")]);
        assert_eq!(config.heap_words, 1024);
        assert_eq!(config.emulator.schedulers, Some(2));
        assert_eq!(config.emulator.max_processes, Some(1000));
        assert_eq!(config.emulator.node_name.as_deref(), Some("beam@host"));
        // `with_emulator` replaces the whole A.6 skeleton, so it is a separate step.
        let replaced = config.with_emulator(EmulatorConfig::default());
        assert_eq!(replaced.emulator, EmulatorConfig::default());
        assert_eq!(replaced.code_paths, vec![PathBuf::from("/code/b")]);
    }

    #[test]
    fn init_args_reports_occurrences_and_missing_flags() {
        let mut args = InitArgs::new();
        assert!(args.is_empty());
        // A valueless flag is one occurrence with no values.
        args.record("noshell", Vec::new());
        assert_eq!(args.get("noshell"), Some(vec![&[][..]]));
        // Two occurrences of the same flag stay separate, in order.
        args.record("pa", vec!["/a".to_string()]);
        args.record("pa", vec!["/b".to_string(), "/c".to_string()]);
        assert_eq!(
            args.get("pa"),
            Some(vec![&["/a".to_string()][..], &["/b".to_string(), "/c".to_string()][..]])
        );
        assert_eq!(args.get("absent"), None);
        assert_eq!(args.len(), 3);
        let occurrences: Vec<(&str, &[String])> = args.occurrences().collect();
        assert_eq!(occurrences.len(), 3);
        assert_eq!(occurrences[0].0, "noshell");
        assert_eq!(occurrences[1].0, "pa");
    }

    #[test]
    fn actions_are_distinguishable() {
        assert_ne!(Action::Boot, Action::Version);
        assert_ne!(
            Action::Eval("2+3".to_string()),
            Action::Eval("2*3".to_string())
        );
        assert_ne!(
            Action::Apply {
                module: "init".to_string(),
                function: "stop".to_string(),
                args: Vec::new(),
            },
            Action::Run {
                module: "init".to_string(),
                function: "stop".to_string(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            Action::RunFile {
                file: PathBuf::from("slice.beam"),
                module: "slice".to_string(),
                function: "add".to_string(),
                arity: 2,
                args: vec![ArgTerm::Int(2), ArgTerm::Int(3)],
            },
            Action::RunFile {
                file: PathBuf::from("slice.beam"),
                module: "slice".to_string(),
                function: "add".to_string(),
                arity: 2,
                args: vec![ArgTerm::Int(2), ArgTerm::Int(3)],
            }
        );
    }
}
