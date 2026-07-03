use std::cell::Cell;
use std::io;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering},
    Arc, Mutex,
};

use bytes::Bytes;
use portable_pty::CommandBuilder;
use ratatui::{layout::Rect, Frame};
#[cfg(test)]
use tokio::sync::watch;
use tokio::sync::{mpsc, Notify};
use tracing::{error, info, warn};

use crate::detect::{Agent, AgentState};
use crate::events::AppEvent;
use crate::layout::PaneId;

#[cfg(test)]
mod fake_terminal;
#[cfg(feature = "termhost")]
mod input_mirror;
mod kitty_keyboard;
mod state;
mod terminal;

use self::terminal::PaneTerminal;
pub(crate) use self::terminal::{TerminalDirtyPatch, TerminalDirtyPatchOutcome};
pub use self::{
    state::PaneState,
    terminal::{InputState, ScrollMetrics, TerminalCursorState},
};

const PANE_TERM: &str = "xterm-256color";
const PANE_COLORTERM: &str = "truecolor";

fn apply_pane_terminal_env(cmd: &mut CommandBuilder) {
    // Each pane is rendered by herdr's own terminal layer, not the outer terminal
    // that launched the app. Advertising the inherited TERM leaks the host terminal
    // identity into shells and across SSH, which breaks redraw and cursor movement
    // when the remote side lacks matching terminfo entries.
    cmd.env("TERM", PANE_TERM);
    cmd.env("COLORTERM", PANE_COLORTERM);
}

#[derive(Clone, Copy, Default)]
struct SpawnInitialState<'a> {
    history_ansi: Option<&'a str>,
}

#[cfg(unix)]
fn usable_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    crate::platform::process_cwd(pid).filter(|cwd| cwd.is_absolute() && cwd.is_dir())
}

#[cfg(unix)]
fn foreground_member_cwd_different_from_shell(
    shell_pid: u32,
    shell_cwd: Option<&std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let job = crate::detect::foreground_job(shell_pid)?;
    for process in job.processes {
        if process.pid == shell_pid {
            continue;
        }
        let Some(cwd) = usable_process_cwd(process.pid) else {
            continue;
        };
        if shell_cwd != Some(&cwd) {
            return Some(cwd);
        }
    }
    None
}

/// Renders a wire-format frame snapshot into a ratatui frame — the shared
/// conversion for termhost panes (daemon-reported frames) and the test fake.
pub(crate) fn render_wire_frame(
    frame: &mut Frame,
    area: Rect,
    show_cursor: bool,
    snapshot: &crate::protocol::FrameData,
    cursor: Option<&crate::protocol::CursorState>,
) {
    let width = snapshot.width as usize;
    {
        let buf = frame.buffer_mut();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &mut buf[(area.x + x, area.y + y)];
                cell.reset();
                if x < snapshot.width && y < snapshot.height {
                    if let Some(data) = snapshot.cells.get((y as usize) * width + (x as usize)) {
                        cell.set_symbol(&data.symbol);
                        cell.fg = crate::protocol::u32_to_color(data.fg);
                        cell.bg = crate::protocol::u32_to_color(data.bg);
                        cell.modifier = crate::protocol::u16_to_modifier(data.modifier);
                    }
                }
            }
        }
    }
    if show_cursor {
        if let Some(cursor) = cursor {
            if cursor.visible && cursor.x < area.width && cursor.y < area.height {
                frame.set_cursor_position((area.x + cursor.x, area.y + cursor.y));
            }
        }
    }
}

/// Builds a dirty patch from a frame snapshot when it has changed since the
/// last collect. Rows are sized exactly to `area_width` (the compositor
/// splices whole rows), padding/truncating against the backend grid as needed.
pub(crate) fn wire_dirty_patch(
    dirty: bool,
    snapshot: impl FnOnce() -> Option<crate::protocol::FrameData>,
    area_width: u16,
    area_height: u16,
) -> TerminalDirtyPatchOutcome {
    if !dirty {
        return TerminalDirtyPatchOutcome::Clean;
    }
    let Some(snapshot) = snapshot() else {
        return TerminalDirtyPatchOutcome::Clean;
    };
    let width = snapshot.width as usize;
    let mut rows = Vec::with_capacity(area_height as usize);
    for y in 0..area_height {
        let mut row = Vec::with_capacity(area_width as usize);
        for x in 0..area_width {
            let cell = if x < snapshot.width && y < snapshot.height {
                snapshot
                    .cells
                    .get((y as usize) * width + (x as usize))
                    .cloned()
                    .unwrap_or_else(wire_blank_cell)
            } else {
                wire_blank_cell()
            };
            row.push(cell);
        }
        rows.push((y, row));
    }
    TerminalDirtyPatchOutcome::Patch(TerminalDirtyPatch { rows })
}

fn wire_blank_cell() -> crate::protocol::CellData {
    crate::protocol::CellData {
        symbol: " ".to_string(),
        fg: 0,
        bg: 0,
        modifier: 0,
        skip: false,
        hyperlink: None,
    }
}

// ---------------------------------------------------------------------------
// PaneRuntime — seam handles for a Go-hosted pane terminal
// ---------------------------------------------------------------------------

/// Runtime handle for a pane whose terminal lives in the Go termhost daemon
/// (PTY, VT emulation, scrollback, and agent detection are all Go-side; WS0
/// stage C removed the in-process emulator). The Rust side keeps only mirrored
/// input modes, pure encoders, and the seam handles. Dropping this closes the
/// daemon-side pane.
pub struct PaneRuntime {
    pane_id: PaneId,
    terminal: Arc<PaneTerminal>,
    io: PaneRuntimeIo,
    current_size: Cell<(u16, u16, u32, u32)>,
    child_pid: Arc<AtomicU32>,
    reported_cwd: Arc<Mutex<Option<std::path::PathBuf>>>,
    kitty_keyboard_flags: Arc<AtomicU16>,
    preserve_processes_on_drop: bool,
    /// True when this runtime adopted a live shell surviving in the persistent
    /// daemon (restart/handoff reconnect) instead of spawning a fresh one.
    adopted_live_shell: bool,
}

enum PaneRuntimeIo {
    #[cfg(feature = "termhost")]
    Termhost(Arc<crate::termhost::TermhostPane>),
    /// Test double (WS0 stage C): input bytes land on `sender`, resizes on
    /// `resize_tx`; the paired [`PaneTerminal::Fake`] answers content queries.
    #[cfg(test)]
    TestChannel {
        sender: mpsc::Sender<Bytes>,
        resize_tx: watch::Sender<(u16, u16, u32, u32)>,
    },
}

impl PaneRuntimeIo {
    /// The Go-backend pane handle, when this runtime is backed by termhost.
    #[cfg(feature = "termhost")]
    fn termhost_pane(&self) -> Option<&Arc<crate::termhost::TermhostPane>> {
        match self {
            PaneRuntimeIo::Termhost(pane) => Some(pane),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => None,
        }
    }

    fn shutdown(&self) {
        match self {
            #[cfg(feature = "termhost")]
            PaneRuntimeIo::Termhost(pane) => {
                use crate::termhost::TerminalBackend;
                pane.close();
            }
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => {}
        }
    }

    /// Whether this runtime is backed by the Go termhost daemon (no local PTY fd).
    /// Such panes survive a live handoff by the replacement reconnecting to the
    /// persistent daemon and adopting the live shell, not by local-PTY fd passing —
    /// so the handoff machinery skips them.
    fn is_termhost(&self) -> bool {
        #[cfg(feature = "termhost")]
        {
            matches!(self, PaneRuntimeIo::Termhost(_))
        }
        #[cfg(not(feature = "termhost"))]
        {
            false
        }
    }

    #[cfg(unix)]
    fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        match self {
            #[cfg(feature = "termhost")]
            PaneRuntimeIo::Termhost(_) => Err(std::io::Error::other(
                "termhost backend has no PTY master fd",
            )),
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { .. } => {
                Err(std::io::Error::other("test runtime has no PTY master fd"))
            }
        }
    }

    #[cfg(unix)]
    fn foreground_process_group_id(&self) -> Option<u32> {
        None
    }

    #[cfg(unix)]
    fn begin_handoff(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        // Handoff pausing was a local-PTY feature; termhost panes survive a
        // handoff by daemon reconnect + adopt instead.
        let _ = timeout;
        Ok(())
    }

    #[cfg(unix)]
    fn set_handoff_paused(&self, paused: bool) -> std::io::Result<()> {
        let _ = paused;
        Ok(())
    }

    #[cfg(unix)]
    fn release_after_commit(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    ) {
        // Go owns the PTY + emulator, so query responses are handled there;
        // the Rust-side `terminal_responses` are empty post-emulator.
        let _ = &terminal_responses;
        match self {
            #[cfg(feature = "termhost")]
            PaneRuntimeIo::Termhost(pane) => {
                use crate::termhost::TerminalBackend;
                pane.resize(rows, cols, cell_width_px, cell_height_px);
            }
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { resize_tx, .. } => {
                let _ = resize_tx.send((rows, cols, cell_width_px, cell_height_px));
            }
        }
    }

    #[cfg(unix)]
    fn nudge_child_redraw_after_handoff(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) {
        let _ = (rows, cols, cell_width_px, cell_height_px);
    }

    async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        match self {
            #[cfg(feature = "termhost")]
            PaneRuntimeIo::Termhost(pane) => {
                use crate::termhost::TerminalBackend;
                pane.write_input(&bytes);
                Ok(())
            }
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { sender, .. } => sender.send(bytes).await,
        }
    }

    fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        match self {
            #[cfg(feature = "termhost")]
            PaneRuntimeIo::Termhost(pane) => {
                use crate::termhost::TerminalBackend;
                pane.write_input(&bytes);
                Ok(())
            }
            #[cfg(test)]
            PaneRuntimeIo::TestChannel { sender, .. } => sender.try_send(bytes),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

impl Drop for PaneRuntime {
    fn drop(&mut self) {
        // Preserved runtimes (handoff export, failed-handoff import rollback,
        // test doubles) must not close the daemon-side pane: closing it kills
        // the very shell being preserved for the session's continuing owner.
        if self.preserve_processes_on_drop {
            return;
        }
        self.io.shutdown();
        shutdown_pane_processes(self.pane_id, self.child_pid.load(Ordering::Acquire), None);
    }
}

fn process_alive_for_shutdown(
    pid: u32,
    child_pid: u32,
    child_wait_completed: bool,
    process_exists: impl FnOnce(u32) -> bool,
) -> bool {
    if pid == child_pid && child_wait_completed {
        return false;
    }
    process_exists(pid)
}

fn wait_for_processes_to_exit(
    pids: &[u32],
    child_pid: u32,
    child_wait_completed: Option<&AtomicBool>,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let child_wait_completed =
            child_wait_completed.is_some_and(|flag| flag.load(Ordering::Acquire));
        if pids.iter().all(|pid| {
            !process_alive_for_shutdown(
                *pid,
                child_pid,
                child_wait_completed,
                crate::platform::process_exists,
            )
        }) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn shutdown_pane_processes(
    pane_id: PaneId,
    child_pid: u32,
    child_wait_completed: Option<&AtomicBool>,
) {
    if child_pid == 0 {
        return;
    }

    let mut pids = crate::platform::session_processes(child_pid);
    if pids.is_empty() {
        pids.push(child_pid);
    }
    pids.sort_unstable();
    pids.dedup();

    for (signal, grace) in [
        (
            crate::platform::Signal::Hangup,
            std::time::Duration::from_millis(250),
        ),
        (
            crate::platform::Signal::Terminate,
            std::time::Duration::from_millis(250),
        ),
        (
            crate::platform::Signal::Kill,
            std::time::Duration::from_millis(250),
        ),
    ] {
        crate::platform::signal_processes(&pids, signal);
        if wait_for_processes_to_exit(&pids, child_pid, child_wait_completed, grace) {
            info!(
                pane = pane_id.raw(),
                pid = child_pid,
                ?signal,
                "pane session terminated"
            );
            return;
        }
    }

    warn!(
        pane = pane_id.raw(),
        pid = child_pid,
        pids = ?pids,
        "pane session still alive after forced shutdown"
    );
}

#[cfg(unix)]
fn truncate_handoff_history(history: String, max_bytes: usize) -> String {
    if history.len() <= max_bytes {
        return history;
    }
    let mut start = history.len().saturating_sub(max_bytes);
    while !history.is_char_boundary(start) {
        start += 1;
    }
    let Some(newline_offset) = history[start..].find('\n') else {
        return String::new();
    };
    start += newline_offset + 1;
    history[start..].to_owned()
}

fn pane_shell(configured_shell: &str) -> String {
    pane_shell_from(configured_shell, std::env::var("SHELL").ok())
}

fn pane_shell_from(configured_shell: &str, env_shell: Option<String>) -> String {
    let configured_shell = configured_shell.trim();
    if !configured_shell.is_empty() {
        return configured_shell.to_string();
    }

    #[cfg(windows)]
    {
        let _ = env_shell;
        default_pane_shell()
    }

    #[cfg(not(windows))]
    env_shell
        .map(|shell| shell.trim().to_string())
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(default_pane_shell)
}

#[cfg(windows)]
fn default_pane_shell() -> String {
    "powershell.exe".into()
}

#[cfg(not(windows))]
fn default_pane_shell() -> String {
    "/bin/sh".into()
}

#[derive(Clone, Copy)]
pub(crate) struct PaneShellConfig<'a> {
    pub(crate) default_shell: &'a str,
    pub(crate) mode: crate::config::ShellModeConfig,
}

impl<'a> PaneShellConfig<'a> {
    pub(crate) fn new(default_shell: &'a str, mode: crate::config::ShellModeConfig) -> Self {
        Self {
            default_shell,
            mode,
        }
    }
}

fn shell_mode_uses_login_shell(
    mode: crate::config::ShellModeConfig,
    target_is_macos: bool,
) -> bool {
    match mode {
        crate::config::ShellModeConfig::Auto => target_is_macos,
        crate::config::ShellModeConfig::Login => true,
        crate::config::ShellModeConfig::NonLogin => false,
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_shell_for_login_mode(shell: &str) -> io::Result<String> {
    if shell.contains(std::path::MAIN_SEPARATOR) {
        let path = Path::new(shell);
        return is_executable_file(path)
            .then(|| shell.to_string())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("login shell {shell:?} is not executable"),
                )
            });
    }

    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(shell))
                .find(|candidate| is_executable_file(candidate))
        })
        .and_then(|path| path.into_os_string().into_string().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("login shell {shell:?} was not found on PATH"),
            )
        })
}

fn pane_shell_command_builder_for_target(
    shell_config: PaneShellConfig<'_>,
    target_is_macos: bool,
) -> io::Result<CommandBuilder> {
    let shell = pane_shell(shell_config.default_shell);
    if shell_mode_uses_login_shell(shell_config.mode, target_is_macos) {
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env("SHELL", resolve_shell_for_login_mode(&shell)?);
        Ok(cmd)
    } else {
        let mut cmd = CommandBuilder::new(&shell);
        apply_windows_powershell_cwd_reporting(&mut cmd, &shell);
        Ok(cmd)
    }
}

fn pane_shell_command_builder(shell_config: PaneShellConfig<'_>) -> io::Result<CommandBuilder> {
    pane_shell_command_builder_for_target(shell_config, cfg!(target_os = "macos"))
}

#[cfg(windows)]
fn apply_windows_powershell_cwd_reporting(cmd: &mut CommandBuilder, shell: &str) {
    if !is_windows_powershell_shell(shell) {
        return;
    }
    cmd.arg("-NoExit");
    cmd.arg("-Command");
    cmd.arg(windows_powershell_cwd_prompt_wrapper());
}

#[cfg(not(windows))]
fn apply_windows_powershell_cwd_reporting(cmd: &mut CommandBuilder, shell: &str) {
    let _ = (cmd, shell);
}

#[cfg(windows)]
fn is_windows_powershell_shell(shell: &str) -> bool {
    let name = Path::new(shell)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(shell)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    )
}

#[cfg(windows)]
fn windows_powershell_cwd_prompt_wrapper() -> &'static str {
    r#"$global:__HERDR_ORIGINAL_PROMPT = if (Test-Path Function:\prompt) { (Get-Command prompt -CommandType Function).ScriptBlock } else { { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " } }; function global:prompt { try { if ($PWD.Provider.Name -eq 'FileSystem') { $uri = ([System.Uri]$PWD.ProviderPath).AbsoluteUri; [Console]::Write("$([char]27)]7;$uri$([char]7)") } } catch {}; & $global:__HERDR_ORIGINAL_PROMPT }"#
}

fn usable_reported_cwd(cwd: std::path::PathBuf) -> Option<std::path::PathBuf> {
    (cwd.is_absolute() && cwd.is_dir()).then_some(cwd)
}

fn publish_reported_cwd(
    pane_id: PaneId,
    cwd: std::path::PathBuf,
    reported_cwd: &Arc<Mutex<Option<std::path::PathBuf>>>,
    events: &mpsc::Sender<AppEvent>,
) {
    let Some(cwd) = usable_reported_cwd(cwd) else {
        return;
    };
    if let Ok(mut current) = reported_cwd.lock() {
        if current.as_ref() == Some(&cwd) {
            return;
        }
        *current = Some(cwd.clone());
    }
    if let Err(err) = events.try_send(AppEvent::TerminalCwdReported { pane_id, cwd }) {
        warn!(
            pane = pane_id.raw(),
            err = %err,
            "failed to send terminal cwd report"
        );
    }
}

impl PaneRuntime {
    pub fn shutdown(mut self) {
        self.io.shutdown();
        shutdown_pane_processes(self.pane_id, self.child_pid.load(Ordering::Acquire), None);
        self.preserve_processes_on_drop = true;
    }

    /// Whether this runtime is backed by the Go termhost daemon (no local PTY fd).
    pub fn is_termhost(&self) -> bool {
        self.io.is_termhost()
    }

    /// Whether this runtime adopted a live shell that survived in the
    /// persistent daemon across a herdr restart or live handoff. Such panes
    /// still host their original process, so launch-argv respawn semantics
    /// carry over (the successor to the fd-import marker).
    pub fn adopted_live_shell(&self) -> bool {
        self.adopted_live_shell
    }

    #[cfg(unix)]
    pub fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        self.io.duplicate_handoff_fd()
    }

    #[cfg(unix)]
    pub fn preserve_for_handoff(mut self) {
        if let Err(err) = self.io.release_after_commit() {
            warn!(
                pane = self.pane_id.raw(),
                err = %err,
                "failed to release pane IO after handoff commit"
            );
        }
        self.preserve_processes_on_drop = true;
    }

    #[cfg(unix)]
    pub fn assume_handoff_ownership(&mut self) {
        self.preserve_processes_on_drop = false;
    }

    #[cfg(unix)]
    pub fn set_handoff_reader_paused(&self, paused: bool) {
        if let Err(err) = self.io.set_handoff_paused(paused) {
            warn!(
                pane = self.pane_id.raw(),
                err = %err,
                paused,
                "failed to update PTY actor handoff pause state"
            );
        }
    }

    #[cfg(unix)]
    pub fn pause_handoff_reader(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        self.io.begin_handoff(timeout)
    }

    #[cfg(unix)]
    pub fn handoff_runtime_state(
        &self,
        pane_id: u32,
    ) -> crate::handoff_runtime::HandoffRuntimeState {
        let child_pid = self.child_pid.load(Ordering::Acquire);
        let (rows, cols, cell_width_px, cell_height_px) = self.current_size.get();
        crate::handoff_runtime::HandoffRuntimeState {
            pane_id,
            child_pid,
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            keyboard_protocol_flags: match self.keyboard_protocol() {
                crate::input::KeyboardProtocol::Legacy => 0,
                crate::input::KeyboardProtocol::Kitty { flags } => flags,
            },
            keyboard_protocol_ansi: self.terminal.kitty_keyboard_state_ansi(),
            input_state: self.input_state(),
            initial_history_ansi: None,
        }
    }

    #[cfg(unix)]
    pub fn handoff_history_ansi(&self) -> Option<String> {
        if self
            .terminal
            .input_state()
            .is_some_and(|input_state| input_state.alternate_screen)
        {
            return None;
        }
        self.snapshot_history().map(|history| {
            truncate_handoff_history(history, crate::server::handoff::MAX_REPLAY_BYTES_PER_PANE)
        })
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        self.terminal.apply_host_terminal_theme(theme);
    }

    pub fn spawn(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        public_pane_id: Option<&str>,
    ) -> std::io::Result<Self> {
        Self::spawn_with_initial_history(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            None,
            events,
            render_notify,
            render_dirty,
            public_pane_id,
        )
    }

    // Runtime construction needs to thread PTY size, environment, theme, and render hooks together.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_with_initial_history(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: PaneShellConfig<'_>,
        initial_history_ansi: Option<&str>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        public_pane_id: Option<&str>,
    ) -> std::io::Result<Self> {
        let mut cmd = pane_shell_command_builder(shell_config)?;
        cmd.cwd(cwd);
        cmd.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
        apply_pane_terminal_env(&mut cmd);
        crate::integration::apply_pane_env(&mut cmd, pane_id, public_pane_id);
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
            cmd,
            "failed to spawn shell",
            SpawnInitialState {
                history_ansi: initial_history_ansi,
            },
        )
    }

    // Runtime construction needs to thread PTY size, environment, theme, and render hooks together.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_shell_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        command: &str,
        extra_env: &[(String, String)],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        public_pane_id: Option<&str>,
    ) -> std::io::Result<Self> {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(command);
        cmd.cwd(cwd);
        cmd.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
        apply_pane_terminal_env(&mut cmd);
        crate::integration::apply_pane_env(&mut cmd, pane_id, public_pane_id);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
            cmd,
            "failed to spawn command pane",
            SpawnInitialState::default(),
        )
    }

    pub fn spawn_argv_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        public_pane_id: Option<&str>,
    ) -> std::io::Result<Self> {
        let Some((program, args)) = argv.split_first() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "argv must not be empty",
            ));
        };
        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        cmd.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
        apply_pane_terminal_env(&mut cmd);
        crate::integration::apply_pane_env(&mut cmd, pane_id, public_pane_id);
        Self::spawn_command_builder(
            pane_id,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
            cmd,
            "failed to spawn argv command pane",
            SpawnInitialState::default(),
        )
    }

    fn spawn_command_builder(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        cmd: CommandBuilder,
        spawn_error_message: &'static str,
        initial_state: SpawnInitialState<'_>,
    ) -> std::io::Result<Self> {
        crate::logging::pane_spawn_started(pane_id.raw(), rows, cols, scrollback_limit_bytes);

        // Unit tests never reach a real daemon: every spawn lands on the
        // channel-backed test double, the successor to the pre-stage-C
        // `cfg(test)` in-process default. Integration/e2e builds (no
        // cfg(test)) take the real termhost path below.
        #[cfg(test)]
        {
            let _ = (host_terminal_theme, spawn_error_message);
            let _ = (&render_notify, &render_dirty);
            // In test builds this early return is the whole fn; the real spawn
            // tail below is cfg'd out.
            #[allow(clippy::needless_return)]
            return Ok(Self::test_spawned_fake(
                pane_id,
                rows,
                cols,
                &cmd,
                initial_state.history_ansi,
                events,
            ));
        }

        // The termhost backend is the only backend (WS0 stage C): the PTY + VT
        // emulation live in the Go daemon; no local emulator is constructed, only
        // a plain-data input-mode mirror (stage B2). An unreachable daemon is a
        // hard error (stage A policy).
        //
        // Unused legacy parameters: the Go daemon owns scrollback, theming, and
        // render scheduling for its panes.
        #[cfg(not(test))]
        {
            let _ = (scrollback_limit_bytes, host_terminal_theme);
            let _ = (&render_notify, &render_dirty);

            #[cfg(not(feature = "termhost"))]
            {
                let _ = (rows, cols, cmd, events, initial_state);
                error!(pane = pane_id.raw(), "{spawn_error_message}");
                Err(io::Error::other(
                "this herdr build has no terminal backend (built without the `termhost` feature)",
            ))
            }

            #[cfg(feature = "termhost")]
            {
                let client = crate::termhost::required_client().inspect_err(|err| {
                    error!(pane = pane_id.raw(), err = %err,
                    "{spawn_error_message}: termhost backend required but unavailable");
                })?;
                let terminal = Arc::new(PaneTerminal::new_mirror());
                let kitty_keyboard_flags = Arc::new(AtomicU16::new(0));
                // If a persistent daemon survived a herdr restart/handoff and still has
                // this pane (reported in welcome.panes), adopt the live shell instead of
                // spawning a fresh one — that's how termhost shells survive a restart.
                // Claiming is one-shot: a respawn with a recycled pane id after the
                // adopted process exits must create a fresh shell.
                let adopt = client.claim_surviving_pane(pane_id.raw());
                Self::finish_termhost(
                    pane_id,
                    rows,
                    cols,
                    terminal,
                    kitty_keyboard_flags,
                    cmd,
                    client,
                    events,
                    initial_state.history_ansi,
                    adopt,
                )
            }
        }
    }

    /// Builds a [`PaneRuntime`] backed by the Go `termhost` daemon instead of an
    /// in-process PTY + ghostty emulator. The local [`PaneTerminal`] is kept but
    /// unfed (emulator-derived queries return empty); display, input, resize, and
    /// exit flow through the Go backend. Richer features (detection text,
    /// selection, scrollback, hyperlinks) await the Go→Rust passthrough events.
    #[cfg(feature = "termhost")]
    #[allow(clippy::too_many_arguments)]
    // In test builds the only caller (the real spawn tail) is cfg'd out.
    #[cfg_attr(test, allow(dead_code))]
    fn finish_termhost(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        terminal: Arc<PaneTerminal>,
        kitty_keyboard_flags: Arc<AtomicU16>,
        cmd: CommandBuilder,
        client: Arc<crate::termhost::TermhostClient>,
        events: mpsc::Sender<AppEvent>,
        initial_history: Option<&str>,
        adopt: bool,
    ) -> std::io::Result<Self> {
        use crate::termhost::{PaneSpec, TerminalBackend};

        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let (command, args) = match argv.split_first() {
            Some((first, rest)) => (first.clone(), rest.to_vec()),
            None => (String::new(), Vec::new()),
        };
        let cwd = cmd
            .get_cwd()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        let env: std::collections::BTreeMap<String, String> = cmd
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();

        // Per-pane signal sink: route the Go backend's out-of-band reports into the
        // same state + AppEvents the in-process path publishes.
        //  - Cwd (OSC 7) → reported_cwd + TerminalCwdReported (new-pane cwd, worktree).
        //  - Agent (Go-side detection) → StateChanged, the same path the Rust screen
        //    detector fed. Go owns detection for termhost panes (the Rust detection
        //    task is not spawned below).
        let reported_cwd: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::new(Mutex::new(None));
        let signal_sink: crate::termhost::SignalSink = {
            let reported_cwd = reported_cwd.clone();
            let events = events.clone();
            let modes_terminal = terminal.clone();
            let modes_kitty_flags = kitty_keyboard_flags.clone();
            Box::new(move |signal| match signal {
                crate::termhost::PaneSignal::Cwd(cwd) => {
                    publish_reported_cwd(
                        pane_id,
                        std::path::PathBuf::from(cwd),
                        &reported_cwd,
                        &events,
                    );
                }
                crate::termhost::PaneSignal::Agent {
                    agent,
                    state,
                    visible_blocker,
                    visible_working,
                } => {
                    let detected = crate::detect::parse_agent_label(&agent);
                    let state = match state.as_str() {
                        "working" => AgentState::Working,
                        "blocked" => AgentState::Blocked,
                        "idle" => AgentState::Idle,
                        _ => AgentState::Unknown,
                    };
                    let _ = events.try_send(AppEvent::StateChanged {
                        pane_id,
                        agent: detected,
                        state,
                        visible_blocker,
                        visible_working,
                        process_exited: false,
                        observed_at: std::time::Instant::now(),
                    });
                }
                crate::termhost::PaneSignal::Clipboard(content) => {
                    // OSC 52 from the Go backend → the same AppEvent the in-process
                    // path emits, so herdr's clipboard writer re-emits it.
                    let _ = events.try_send(AppEvent::ClipboardWrite { content });
                }
                crate::termhost::PaneSignal::Title(title) => {
                    // OSC 0/2 window title → terminal chrome. Empty clears it.
                    let _ = events.try_send(AppEvent::TerminalTitleReported {
                        pane_id,
                        title: (!title.is_empty()).then_some(title),
                    });
                }
                crate::termhost::PaneSignal::Modes(modes) => {
                    // Mirror the program's input modes onto the (unfed) local emulator
                    // so its key/mouse encoders and input routing match the program.
                    modes_terminal.apply_input_modes(&modes);
                    modes_kitty_flags.store(modes.kitty_keyboard_flags, Ordering::Relaxed);
                }
            })
        };

        // Adopt a surviving live shell (reconnect after restart/handoff) vs. spawn a
        // fresh one. Adoption skips CreatePane (and its cwd/command/initial_history,
        // which only seed a *new* shell) and requests a resync so the pane repaints.
        let pane = if adopt {
            client.adopt_pane(pane_id.raw(), Some(signal_sink))
        } else {
            client.create_pane(
                PaneSpec {
                    pane_id: pane_id.raw(),
                    cols,
                    rows,
                    cell_width_px: 0,
                    cell_height_px: 0,
                    cwd,
                    command,
                    args,
                    env,
                    initial_history: initial_history.unwrap_or_default().to_string(),
                },
                Some(signal_sink),
            )
        }
        .map_err(|err| std::io::Error::other(err.to_string()))?;
        let pane = Arc::new(pane);

        // Exit watcher: the client's reader thread records the exit code from the
        // Go `pane_exited` event; surface it as PaneDied like the in-process child
        // watcher does.
        {
            let pane = pane.clone();
            let events = events.clone();
            tokio::spawn(async move {
                loop {
                    if let Some(code) = pane.exit_status() {
                        crate::logging::pane_exited(pane_id.raw(), &format!("exit_code={code}"));
                        if let Err(err) = events.send(AppEvent::PaneDied { pane_id }).await {
                            error!(pane = pane_id.raw(), err = %err, "failed to send PaneDied event");
                        }
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            });
        }

        // Detection for termhost panes runs in the Go daemon, reported via the
        // Agent signal above; no Rust-side detection task exists (WS0 stage C).
        let child_pid = Arc::new(AtomicU32::new(0));

        Ok(Self {
            pane_id,
            terminal,
            io: PaneRuntimeIo::Termhost(pane),
            current_size: Cell::new((rows, cols, 0, 0)),
            child_pid,
            reported_cwd,
            kitty_keyboard_flags,
            preserve_processes_on_drop: false,
            adopted_live_shell: adopt,
        })
    }

    /// No-op since WS0 stage C: agent detection runs in the Go daemon
    /// (`internal/detect`), which owns release/reset/lifecycle suppression.
    /// The methods remain so app-level agent lifecycle code keeps one call
    /// shape until that logic moves Go-side.
    pub fn begin_graceful_release(&self, agent: Agent) {
        let _ = agent;
    }

    /// No-op since WS0 stage C — see [`Self::begin_graceful_release`].
    pub fn reset_agent_detection(&self) {}

    /// No-op since WS0 stage C — see [`Self::begin_graceful_release`].
    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        let _ = active;
    }

    pub(crate) fn current_size(&self) -> (u16, u16) {
        let (rows, cols, _, _) = self.current_size.get();
        (rows, cols)
    }

    /// Resize if the dimensions actually changed.
    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        let rows = rows.max(2);
        let cols = cols.max(4);
        let size = (rows, cols, cell_width_px, cell_height_px);
        if self.current_size.get() == size {
            return;
        }
        self.current_size.set(size);
        let terminal_responses = self
            .terminal
            .resize(rows, cols, cell_width_px, cell_height_px);
        self.io.resize(
            rows,
            cols,
            cell_width_px,
            cell_height_px,
            terminal_responses,
        );
    }

    #[cfg(unix)]
    pub fn nudge_child_redraw_after_handoff(&self) {
        let (rows, cols, cell_width_px, cell_height_px) = self.current_size.get();
        self.io
            .nudge_child_redraw_after_handoff(rows, cols, cell_width_px, cell_height_px);
    }

    /// Scroll up by N lines (into scrollback history).
    pub fn scroll_up(&self, lines: usize) {
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            pane.scroll(-(lines.min(i32::MAX as usize) as i32));
            return;
        }
        self.terminal.scroll_up(lines);
    }

    /// Scroll down by N lines (toward live output).
    pub fn scroll_down(&self, lines: usize) {
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            pane.scroll(lines.min(i32::MAX as usize) as i32);
            return;
        }
        self.terminal.scroll_down(lines);
    }

    /// Reset scroll to live view (offset = 0).
    pub fn scroll_reset(&self) {
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            pane.scroll(i32::MAX); // a large positive delta; the Go side clamps to the bottom
            return;
        }
        self.terminal.scroll_reset();
    }

    /// Set scrollback offset measured from the live bottom of the terminal.
    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            // Convert the absolute target to a delta from the last reported offset.
            // Seam delta is positive=down (toward bottom), so delta = current - target.
            let current = pane.scroll_metrics().map_or(0, |m| m.offset_from_bottom) as i64;
            let delta = (current - lines as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            pane.scroll(delta);
            return;
        }
        self.terminal.set_scroll_offset_from_bottom(lines);
    }

    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            return pane.scroll_metrics().map(|m| ScrollMetrics {
                offset_from_bottom: m.offset_from_bottom,
                max_offset_from_bottom: m.max_offset_from_bottom,
                viewport_rows: m.viewport_rows,
            });
        }
        self.terminal.scroll_metrics()
    }

    pub fn input_state(&self) -> Option<InputState> {
        self.terminal.input_state()
    }

    pub fn cursor_state(&self, area: Rect, show_cursor: bool) -> Option<TerminalCursorState> {
        if !show_cursor {
            return None;
        }
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            let cursor = pane.cursor()?;
            if cursor.x >= area.width || cursor.y >= area.height {
                return None;
            }
            return Some(TerminalCursorState {
                x: area.x + cursor.x,
                y: area.y + cursor.y,
                visible: cursor.visible,
                shape: cursor.shape,
            });
        }
        let cursor = self.terminal.cursor_state()?;
        if cursor.x >= area.width || cursor.y >= area.height {
            return None;
        }
        Some(TerminalCursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.terminal.synchronized_output_active()
    }

    /// For a termhost pane, reads buffer text from the Go backend (the local
    /// emulator is unfed); `None` for in-process panes or on failure. `lines`
    /// saturates to u32 — usize::MAX (snapshot_history) lands above the buffer size,
    /// which the Go side reads as "whole buffer".
    #[cfg(feature = "termhost")]
    fn termhost_text(&self, scope: u8, lines: usize, ansi: bool, unwrap: bool) -> Option<String> {
        let pane = self.io.termhost_pane()?;
        pane.extract_text_blocking(scope, lines.min(u32::MAX as usize) as u32, ansi, unwrap)
    }

    pub fn visible_text(&self) -> String {
        #[cfg(feature = "termhost")]
        if let Some(t) = self.termhost_text(crate::termhost::TEXT_SCOPE_VISIBLE, 0, false, false) {
            return t;
        }
        self.terminal.visible_text()
    }

    pub fn visible_ansi(&self) -> String {
        #[cfg(feature = "termhost")]
        if let Some(t) = self.termhost_text(crate::termhost::TEXT_SCOPE_VISIBLE, 0, true, false) {
            return t;
        }
        self.terminal.visible_ansi()
    }

    pub fn detection_text(&self) -> String {
        // Go owns detection for termhost panes; this read-API source maps to the
        // visible screen text.
        #[cfg(feature = "termhost")]
        if let Some(t) = self.termhost_text(crate::termhost::TEXT_SCOPE_VISIBLE, 0, false, false) {
            return t;
        }
        self.terminal.detection_text()
    }

    pub fn agent_osc_title(&self) -> String {
        self.terminal.agent_osc_title()
    }

    pub fn agent_osc_progress(&self) -> String {
        self.terminal.agent_osc_progress()
    }

    pub fn recent_text(&self, lines: usize) -> String {
        #[cfg(feature = "termhost")]
        if let Some(t) = self.termhost_text(crate::termhost::TEXT_SCOPE_RECENT, lines, false, false)
        {
            return t;
        }
        self.terminal.recent_text(lines)
    }

    pub fn recent_ansi(&self, lines: usize) -> String {
        #[cfg(feature = "termhost")]
        if let Some(t) = self.termhost_text(crate::termhost::TEXT_SCOPE_RECENT, lines, true, false)
        {
            return t;
        }
        self.terminal.recent_ansi(lines)
    }

    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        #[cfg(feature = "termhost")]
        if let Some(t) = self.termhost_text(crate::termhost::TEXT_SCOPE_RECENT, lines, false, true)
        {
            return t;
        }
        self.terminal.recent_unwrapped_text(lines)
    }

    pub fn recent_unwrapped_ansi(&self, lines: usize) -> String {
        #[cfg(feature = "termhost")]
        if let Some(t) = self.termhost_text(crate::termhost::TEXT_SCOPE_RECENT, lines, true, true) {
            return t;
        }
        self.terminal.recent_unwrapped_ansi(lines)
    }

    pub fn snapshot_history(&self) -> Option<String> {
        let ansi = self.recent_unwrapped_ansi(usize::MAX);
        (!ansi.trim().is_empty()).then_some(ansi)
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        // Termhost panes keep an unfed local emulator, so read the selection from
        // the Go backend with a blocking request/response over the seam. This serves
        // every selection path uniformly (drag copy, double-click word, URL detect).
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            let ((anchor_row, anchor_col), (cursor_row, cursor_col)) = selection.ordered_cells();
            return pane
                .extract_selection_blocking(anchor_row, anchor_col, cursor_row, cursor_col, false);
        }
        self.terminal.extract_selection(selection)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            if let Some(snapshot) = pane.snapshot() {
                render_wire_frame(frame, area, show_cursor, &snapshot, pane.cursor().as_ref());
            }
            return;
        }
        self.terminal.render(frame, area, show_cursor);
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            return wire_dirty_patch(
                pane.take_dirty(),
                || pane.snapshot(),
                area_width,
                area_height,
            );
        }
        self.terminal.collect_dirty_patch(area_width, area_height)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        // Termhost panes keep an unfed local emulator; resolve links from the
        // Go-fed frame grid (which carries the OSC 8 URI table) instead.
        #[cfg(feature = "termhost")]
        if let Some(pane) = self.io.termhost_pane() {
            return pane.visible_hyperlinks(area.x, area.y, area.width, area.height);
        }
        self.terminal.visible_hyperlinks(area)
    }

    pub fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::terminal::types::KittyImagePlacement>
    where
        F: FnMut(crate::terminal::types::KittyImageDescriptor) -> bool,
    {
        self.terminal
            .kitty_image_placements_with_data_filter(needs_data)
    }

    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        let fallback = crate::input::KeyboardProtocol::from_kitty_flags(
            self.kitty_keyboard_flags.load(Ordering::Relaxed),
        );
        self.terminal.keyboard_protocol(fallback)
    }

    pub fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        self.terminal
            .encode_terminal_key(key, self.keyboard_protocol())
    }

    pub async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        self.io.send_bytes(bytes).await
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.io.try_send_bytes(bytes)
    }

    pub async fn send_paste(&self, text: String) -> Result<(), mpsc::error::SendError<Bytes>> {
        let bracketed = self
            .input_state()
            .map(|state| state.bracketed_paste)
            .unwrap_or(false);
        let payload = if bracketed {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        self.send_bytes(Bytes::from(payload)).await
    }

    pub fn try_send_focus_event(&self, event: crate::terminal::types::FocusEvent) -> bool {
        if !self
            .input_state()
            .map(|state| state.focus_reporting)
            .unwrap_or(false)
        {
            return false;
        }

        let bytes = crate::terminal::types::encode_focus(event);
        if let Err(err) = self.try_send_bytes(Bytes::from(bytes)) {
            warn!(err = %err, ?event, "failed to forward pane focus event");
        }
        true
    }

    pub fn wheel_routing(&self) -> Option<WheelRouting> {
        self.terminal.wheel_routing()
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if !self.input_state()?.mouse_protocol_mode.reporting_enabled() {
            return None;
        }
        self.terminal
            .encode_mouse_button(kind, column, row, modifiers)
    }

    pub fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal
            .encode_mouse_motion(kind, column, row, modifiers)
    }

    pub fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing()? != WheelRouting::MouseReport {
            return None;
        }
        self.terminal
            .encode_mouse_wheel(kind, column, row, modifiers)
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        self.input_state()?;
        if self.wheel_routing()? != WheelRouting::AlternateScroll {
            return None;
        }
        let key = match kind {
            crossterm::event::MouseEventKind::ScrollUp => crossterm::event::KeyCode::Up,
            crossterm::event::MouseEventKind::ScrollDown => crossterm::event::KeyCode::Down,
            _ => return None,
        };
        Some(self.encode_terminal_key(crate::input::TerminalKey::new(
            key,
            crossterm::event::KeyModifiers::empty(),
        )))
    }

    /// Get the current working directory of the child shell process.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        if let Some(cwd) = self
            .reported_cwd
            .lock()
            .ok()
            .and_then(|reported_cwd| reported_cwd.clone())
            .and_then(usable_reported_cwd)
        {
            return Some(cwd);
        }
        let pid = self.child_pid.load(Ordering::Relaxed);
        crate::platform::process_cwd(pid)
    }

    /// Get the current working directory of the process group controlling the pane PTY.
    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        #[cfg(unix)]
        {
            let pid = self.child_pid.load(Ordering::Acquire);
            let shell_cwd = usable_process_cwd(pid);
            let foreground_pgid = self
                .io
                .foreground_process_group_id()
                .or_else(|| crate::platform::foreground_process_group_id(pid));
            let leader_cwd = foreground_pgid.and_then(usable_process_cwd);

            if leader_cwd.as_ref() == shell_cwd.as_ref() {
                foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref()).or(leader_cwd)
            } else {
                leader_cwd
                    .or_else(|| foreground_member_cwd_different_from_shell(pid, shell_cwd.as_ref()))
            }
        }

        #[cfg(not(unix))]
        {
            None
        }
    }
}

#[cfg(test)]
impl PaneRuntime {
    /// What `spawn*` hands back under `cfg(test)`: a channel-backed double in
    /// place of a daemon pane (successor to the pre-stage-C in-process
    /// default). Seeds the requested cwd (as the daemon's OSC 7 report would)
    /// and any restore history, and keeps the input channel drained like a
    /// live PTY would.
    fn test_spawned_fake(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cmd: &CommandBuilder,
        initial_history_ansi: Option<&str>,
        events: mpsc::Sender<AppEvent>,
    ) -> Self {
        let history = initial_history_ansi.unwrap_or_default().as_bytes().to_vec();
        let (mut runtime, mut rx) =
            Self::test_with_channel_and_scrollback_bytes(cols, rows, 1 << 20, &history, 64);
        runtime.pane_id = pane_id;
        if let Some(cwd) = cmd.get_cwd() {
            if let Ok(mut reported) = runtime.reported_cwd.lock() {
                *reported = Some(std::path::PathBuf::from(cwd));
            }
        }

        // Emulate tty echo: feed written input back into the terminal content
        // so tests can observe injected commands in history, as they would
        // with a live shell.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let terminal = runtime.terminal.clone();
            handle.spawn(async move {
                let (echo_tx, _echo_rx) = mpsc::channel(1);
                while let Some(bytes) = rx.recv().await {
                    let _ = terminal.process_pty_bytes(pane_id, 0, &bytes, &echo_tx);
                }
            });
        }

        // An explicit command (`sh -c …`, editor/agent argv) is executed as a
        // plain subprocess so its side effects and exit → PaneDied behave like
        // the deleted in-process path. A bare interactive shell stays inert —
        // without a PTY it would exit immediately and tear the pane down.
        let argv: Vec<std::ffi::OsString> = cmd.get_argv().to_vec();
        if !cmd.is_default_prog() && argv.len() > 1 {
            let mut command = std::process::Command::new(&argv[0]);
            command
                .args(&argv[1..])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some(cwd) = cmd.get_cwd() {
                command.current_dir(cwd);
            }
            for (key, value) in cmd.iter_extra_env_as_str() {
                command.env(key, value);
            }
            if let Ok(mut child) = command.spawn() {
                // Deliberately NOT recorded in child_pid: the subprocess shares
                // the test runner's session, and shutdown_pane_processes would
                // signal that whole session (i.e. kill `cargo test`).
                std::thread::spawn(move || {
                    let _ = child.wait();
                    let _ = events.blocking_send(AppEvent::PaneDied { pane_id });
                });
            }
        }
        runtime
    }

    pub(crate) fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, 0, &[], 4)
    }

    pub(crate) fn test_with_channel_capacity(
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, 0, &[], capacity)
    }

    pub(crate) fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::test_with_scrollback_bytes(cols, rows, 0, bytes)
    }

    pub(crate) fn test_process_pty_bytes(&self, bytes: &[u8]) {
        let (tx, _rx) = mpsc::channel(1);
        let _ = self.terminal.process_pty_bytes(self.pane_id, 0, bytes, &tx);
    }

    pub(crate) fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, scrollback_limit_bytes, bytes, 4).0
    }

    pub(crate) fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (tx, rx) = mpsc::channel(channel_capacity);
        let (resize_tx, _resize_rx) = watch::channel((rows, cols, 0, 0));
        let terminal = PaneTerminal::new_fake(cols, rows, scrollback_limit_bytes);
        let (feed_tx, _feed_rx) = mpsc::channel(1);
        let _ = terminal.process_pty_bytes(PaneId::from_raw(0), 0, bytes, &feed_tx);

        (
            Self {
                pane_id: PaneId::from_raw(0),
                terminal: Arc::new(terminal),
                io: PaneRuntimeIo::TestChannel {
                    sender: tx,
                    resize_tx,
                },
                current_size: Cell::new((rows, cols, 0, 0)),
                child_pid: Arc::new(AtomicU32::new(0)),
                reported_cwd: Arc::new(Mutex::new(None)),
                kitty_keyboard_flags: Arc::new(AtomicU16::new(0)),
                preserve_processes_on_drop: true,
                adopted_live_shell: false,
            },
            rx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_liveness_treats_reaped_direct_child_as_gone() {
        assert!(!process_alive_for_shutdown(42, 42, true, |_| true));
    }

    #[test]
    fn shutdown_liveness_keeps_unreaped_direct_child_alive() {
        assert!(process_alive_for_shutdown(42, 42, false, |_| true));
    }

    #[test]
    fn shutdown_liveness_keeps_other_session_processes_alive() {
        assert!(process_alive_for_shutdown(43, 42, true, |_| true));
    }

    #[test]
    fn shutdown_liveness_treats_missing_process_as_gone() {
        assert!(!process_alive_for_shutdown(43, 42, false, |_| false));
    }

    #[test]
    fn pane_shell_prefers_configured_shell() {
        assert_eq!(
            pane_shell_from("/usr/bin/nu", Some("/bin/bash".to_string())),
            "/usr/bin/nu"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn pane_shell_falls_back_to_shell_env() {
        assert_eq!(
            pane_shell_from("", Some("/bin/bash".to_string())),
            "/bin/bash"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pane_shell_ignores_shell_env_on_windows() {
        assert_eq!(
            pane_shell_from("", Some("c:\\windows\\system32\\cmd.exe".to_string())),
            default_pane_shell()
        );
    }

    #[test]
    fn pane_shell_ignores_empty_values() {
        assert_eq!(
            pane_shell_from("   ", Some("  ".to_string())),
            default_pane_shell()
        );
        assert_eq!(pane_shell_from("", None), default_pane_shell());
    }

    #[test]
    fn shell_mode_auto_uses_login_shell_only_on_macos() {
        assert!(shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::Auto,
            true
        ));
        assert!(!shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::Auto,
            false
        ));
        assert!(shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::Login,
            false
        ));
        assert!(!shell_mode_uses_login_shell(
            crate::config::ShellModeConfig::NonLogin,
            true
        ));
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_builder_uses_default_prog_with_resolved_shell_env() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::Login),
            false,
        )
        .unwrap();
        assert!(cmd.is_default_prog());
        assert_eq!(
            cmd.get_env("SHELL").and_then(std::ffi::OsStr::to_str),
            Some("/bin/sh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn auto_shell_builder_uses_login_shell_on_macos_target() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::Auto),
            true,
        )
        .unwrap();
        assert!(cmd.is_default_prog());
        assert_eq!(
            cmd.get_env("SHELL").and_then(std::ffi::OsStr::to_str),
            Some("/bin/sh")
        );
    }

    #[test]
    fn auto_shell_builder_keeps_direct_shell_on_non_macos_target() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::Auto),
            false,
        )
        .unwrap();
        assert!(!cmd.is_default_prog());
        assert_eq!(cmd.get_argv(), &[std::ffi::OsString::from("/bin/sh")]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_powershell_shell_builder_wraps_cwd_reporting_prompt() {
        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("powershell.exe", crate::config::ShellModeConfig::NonLogin),
            false,
        )
        .unwrap();
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(argv[0], "powershell.exe");
        assert!(argv.iter().any(|arg| arg == "-NoExit"));
        assert!(argv
            .iter()
            .any(|arg| arg.contains("]7;") && arg.contains("Function:\\prompt")));
    }

    #[test]
    fn login_shell_builder_rejects_missing_shell_instead_of_falling_back() {
        let err = pane_shell_command_builder_for_target(
            PaneShellConfig::new(
                "/__herdr_missing_shell__",
                crate::config::ShellModeConfig::Login,
            ),
            false,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_builder_resolves_bare_shell_names_from_path() {
        let _lock = crate::integration::integration_env_lock();
        let base = std::env::temp_dir().join(format!(
            "herdr-login-shell-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let shell = bin.join("fake-shell");
        std::fs::write(&shell, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let original_path = std::env::var_os("PATH");
        std::env::set_var("PATH", &bin);

        let cmd = pane_shell_command_builder_for_target(
            PaneShellConfig::new("fake-shell", crate::config::ShellModeConfig::Login),
            false,
        )
        .unwrap();

        assert!(cmd.is_default_prog());
        assert_eq!(
            cmd.get_env("SHELL").and_then(std::ffi::OsStr::to_str),
            shell.to_str()
        );
        match original_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_resolution_preserves_shell_paths() {
        assert_eq!(resolve_shell_for_login_mode("/bin/sh").unwrap(), "/bin/sh");
    }

    #[test]
    fn non_login_shell_builder_execs_resolved_shell_directly() {
        let cmd = pane_shell_command_builder(PaneShellConfig::new(
            "/bin/sh",
            crate::config::ShellModeConfig::NonLogin,
        ))
        .unwrap();
        assert!(!cmd.is_default_prog());
        assert_eq!(cmd.get_argv(), &[std::ffi::OsString::from("/bin/sh")]);
    }

    #[cfg(unix)]
    #[test]
    fn truncate_handoff_history_keeps_recent_utf8_boundary() {
        let history = format!("old\n{}\nrecent\n", "é".repeat(8));

        let truncated = truncate_handoff_history(history, 20);

        assert_eq!(truncated, "recent\n");
        assert!(truncated.is_char_boundary(0));
    }

    #[cfg(unix)]
    #[test]
    fn truncate_handoff_history_drops_partial_long_line() {
        let history = format!("old\n{}", "x".repeat(64));

        let truncated = truncate_handoff_history(history, 12);

        assert!(truncated.is_empty());
    }

    #[tokio::test]
    async fn focus_events_are_forwarded_when_enabled() {
        let (runtime, mut rx) = PaneRuntime::test_with_channel(80, 24);
        runtime.test_process_pty_bytes(b"\x1b[?1004h");

        assert!(runtime.try_send_focus_event(crate::terminal::types::FocusEvent::Gained));
        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"\x1b[I"));
    }

    #[tokio::test]
    async fn focus_events_are_suppressed_when_disabled() {
        let (runtime, mut rx) = PaneRuntime::test_with_channel(80, 24);

        assert!(!runtime.try_send_focus_event(crate::terminal::types::FocusEvent::Gained));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }
}
