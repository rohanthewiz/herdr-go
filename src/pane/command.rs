//! Pane launch specification (WS0 stage D).
//!
//! Replaces `portable_pty::CommandBuilder` now that no local PTY exists: the
//! Rust side only *describes* what to launch — program/argv, cwd, and extra
//! env — and `finish_termhost` serializes it into the Go daemon's `PaneSpec`.
//! The daemon owns spawning, sessions, and signal handling.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

/// A pane command description: argv (empty = the user's default shell, chosen
/// daemon-side from the `SHELL` env we pass), working directory, and extra
/// environment applied over the daemon's own.
#[derive(Debug, Clone, Default)]
pub(crate) struct CommandBuilder {
    /// argv[0] is the program; empty means "default program" (login shell).
    argv: Vec<OsString>,
    cwd: Option<OsString>,
    env: BTreeMap<OsString, OsString>,
}

impl CommandBuilder {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        CommandBuilder {
            argv: vec![program.as_ref().to_owned()],
            ..CommandBuilder::default()
        }
    }

    /// The user's default shell, resolved by the spawning side from the
    /// `SHELL` env var (see `pane_shell_command_builder_for_target`, which
    /// always sets it for this form).
    pub fn new_default_prog() -> Self {
        CommandBuilder::default()
    }

    // Prod converts argv wholesale in finish_termhost; these accessors serve
    // the shell-builder unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_default_prog(&self) -> bool {
        self.argv.is_empty()
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) {
        debug_assert!(
            !self.argv.is_empty(),
            "args cannot be appended to the default program"
        );
        self.argv.push(arg.as_ref().to_owned());
    }

    pub fn cwd(&mut self, cwd: impl AsRef<OsStr>) {
        self.cwd = Some(cwd.as_ref().to_owned());
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        self.env
            .insert(key.as_ref().to_owned(), value.as_ref().to_owned());
    }

    pub fn get_argv(&self) -> &[OsString] {
        &self.argv
    }

    pub fn get_cwd(&self) -> Option<&OsString> {
        self.cwd.as_ref()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get_env(&self, key: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.env.get(key.as_ref()).map(OsString::as_os_str)
    }

    /// The extra environment as UTF-8 pairs (non-UTF-8 entries are skipped —
    /// the seam protocol carries strings).
    pub fn iter_extra_env_as_str(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env
            .iter()
            .filter_map(|(k, v)| Some((k.to_str()?, v.to_str()?)))
    }
}
