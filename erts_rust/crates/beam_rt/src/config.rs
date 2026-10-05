//! The runtime configuration: the parsed emulator switches plus the selected action.
//!
//! [`RuntimeConfig`] wraps the `beam_common` A.6 **emulator skeleton** ([`EmulatorConfig`])
//! with the two things only `beam_rt` owns: the **code path** (`-pa`/`-pz`) and the
//! **action** the CLI was asked to perform (`-version`, `-eval`, `-s`, `run`, …).
//! The parser that fills it in lives in [`crate::cli`].

use std::path::PathBuf;

use beam_term::Eterm;

use crate::config::Action::{Boot, Version};

/// The emulator start-up surface — `beam_common`'s A.6 configuration skeleton.
///
/// `beam_rt` re-exports the type under this name because from the runtime's point of
/// view it *is* the emulator configuration; the full `+`/`-`/`-M*` parser lives here.
pub type EmulatorConfig = beam_common::RuntimeConfig;

/// The default process-heap size in term words (64 Ki words — ample for the
/// Milestone-1 vertical slice; there is no collector yet, see `runtime.rs`).
pub const DEFAULT_HEAP_WORDS: usize = 65_536;

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
    /// Evaluate the documented integer-expression subset and print the result
    /// (`-eval Expr`).
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
    /// Load a `.beam` file, run `module:function/arity` and print the result
    /// (`run <file.beam> <mod>:<func>/<arity> [int-args…]`).
    RunFile {
        /// The `.beam` file to load.
        file: PathBuf,
        /// The module the file is expected to define.
        module: String,
        /// The function to run.
        function: String,
        /// The arity to run.
        arity: u32,
        /// The integer arguments.
        args: Vec<Eterm>,
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
    /// The action to perform.
    pub action: Action,
}

impl Default for RuntimeConfig {
    /// The defaults of a bare `beam_rt` invocation: a `jit` emulator, an empty code
    /// path, a [`DEFAULT_HEAP_WORDS`]-word heap and [`Action::Boot`].
    fn default() -> Self {
        RuntimeConfig {
            emulator: EmulatorConfig::default(),
            code_paths: Vec::new(),
            heap_words: DEFAULT_HEAP_WORDS,
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

    use beam_term::make_small;

    use super::*;

    #[test]
    fn defaults() {
        let config = RuntimeConfig::default();
        assert_eq!(config, RuntimeConfig::new());
        assert_eq!(config.emulator, EmulatorConfig::default());
        assert!(config.code_paths.is_empty());
        assert_eq!(config.heap_words, DEFAULT_HEAP_WORDS);
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
    fn actions_are_distinguishable() {
        assert_ne!(Action::Boot, Action::Version);
        assert_ne!(
            Action::Eval("2+3".to_string()),
            Action::Eval("2*3".to_string())
        );
        assert_eq!(
            Action::RunFile {
                file: PathBuf::from("slice.beam"),
                module: "slice".to_string(),
                function: "add".to_string(),
                arity: 2,
                args: vec![make_small(2), make_small(3)],
            },
            Action::RunFile {
                file: PathBuf::from("slice.beam"),
                module: "slice".to_string(),
                function: "add".to_string(),
                arity: 2,
                args: vec![make_small(2), make_small(3)],
            }
        );
    }
}
