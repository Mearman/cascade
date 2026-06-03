//! `cascade service` — manage the Cascade daemon as an OS background service.
//!
//! The default and core scope is per-user, requiring no administrator rights:
//! a launchd `LaunchAgent` on macOS, a systemd `--user` unit on Linux, and a
//! logon Scheduled Task on Windows. Each writes only into the user's own
//! directories and registers through the user-scoped OS command, so no
//! elevation is ever needed.
//!
//! The architecture keeps two halves cleanly apart:
//!
//! * a **pure generator** — each platform module exposes a `generate` free
//!   function that turns a [`ServiceSpec`] into the platform's
//!   service-definition text (plist / unit / task XML). It is plain string work
//!   with no OS calls, compiled and unit-tested on every host regardless of
//!   `target_os`.
//! * a **platform adapter** — the [`ServiceManager`] `install` / `uninstall` /
//!   `start` / `stop` / `status` methods write the generated file and drive the
//!   OS register command. Only this half is `cfg(target_os)`-gated.
//!
//! The `System` scope is scaffolded — the [`ServiceScope`] enum and the
//! manager contract both admit it — but its platform backends are deferred in
//! this pass. Only the no-admin `User` scope is implemented.

use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use super::CliContext;

mod linux;
mod macos;
mod windows;

/// The label the service is registered under across every platform.
///
/// Reverse-DNS form so it sits cleanly in launchd's domain namespace and reads
/// unambiguously in systemd and Task Scheduler. The same string is the launchd
/// `Label`, the systemd unit stem, and the Scheduled Task name.
pub const SERVICE_LABEL: &str = "io.cascade.daemon";

/// Which OS scope a service is installed into.
///
/// `User` is the per-user, no-admin scope implemented in this pass. `System`
/// is the machine-wide scope; it is part of the contract so callers and
/// generators can address it, but its platform backends are deferred — the
/// adapters reject it until they are built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceScope {
    /// Per-user service in the user's own directories; no elevation.
    User,
    /// Machine-wide service; requires elevation. Scaffolded, not yet built.
    System,
}

impl ServiceScope {
    /// A short human label for the scope, for printing the chosen scope.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::System => "system",
        }
    }
}

/// The serialisable description a platform generator needs to emit a service
/// definition.
///
/// Built once from [`CliContext`] by [`ServiceSpec::from_context`]. It names the
/// program to run (the cascade binary plus its arguments), the registration
/// label, where stdout/stderr are logged, the working directory, and the two
/// lifecycle switches every platform expresses in its own dialect: keep the
/// daemon alive if it exits, and start it automatically at load / logon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Registration label / unit stem / task name.
    pub label: String,
    /// Absolute path to the `cascade` executable to run.
    pub program: PathBuf,
    /// Arguments passed to the program — `["start"]` for the daemon.
    pub args: Vec<String>,
    /// File the daemon's stdout is redirected to.
    pub stdout_log: PathBuf,
    /// File the daemon's stderr is redirected to.
    pub stderr_log: PathBuf,
    /// Working directory the daemon runs in (the config directory).
    pub working_dir: PathBuf,
    /// Restart the daemon if it exits.
    pub keep_alive: bool,
    /// Start the daemon automatically at service load / user logon.
    pub run_at_load: bool,
}

impl ServiceSpec {
    /// Build the spec from the shared CLI context.
    ///
    /// The program is the currently-running `cascade` executable, resolved via
    /// [`std::env::current_exe`] so the installed service points at exactly the
    /// binary the operator invoked rather than guessing a path. Logs and the
    /// working directory live under the config directory so a per-user install
    /// touches only the user's own tree.
    ///
    /// # Errors
    ///
    /// Returns an error if the current executable path cannot be resolved.
    pub fn from_context(ctx: &CliContext) -> Result<Self> {
        let program = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("could not resolve the cascade executable path: {e}"))?;
        Ok(Self {
            label: SERVICE_LABEL.to_owned(),
            program,
            args: vec!["start".to_owned()],
            stdout_log: ctx.config_dir.join("cascade.out.log"),
            stderr_log: ctx.config_dir.join("cascade.err.log"),
            working_dir: ctx.config_dir.clone(),
            keep_alive: true,
            run_at_load: true,
        })
    }
}

/// A platform's service manager: the OS-facing lifecycle adapter.
///
/// Each platform pairs this adapter with a module-level `generate` free
/// function that renders the service-definition text. That generator is
/// host-independent string work, compiled and unit-tested on every host; the
/// adapter's `install` writes its output before registering it with the OS.
/// The lifecycle methods here are the `cfg(target_os)`-gated half that touches
/// the filesystem and drives the OS register / control commands.
#[async_trait]
pub trait ServiceManager: Send + Sync {
    /// Write the service definition and register it with the OS.
    async fn install(&self, spec: &ServiceSpec) -> Result<()>;

    /// Deregister the service and remove its definition file.
    async fn uninstall(&self, spec: &ServiceSpec) -> Result<()>;

    /// Start the registered service.
    async fn start(&self, spec: &ServiceSpec) -> Result<()>;

    /// Stop the registered service.
    async fn stop(&self, spec: &ServiceSpec) -> Result<()>;

    /// Report whether the service is registered and running.
    async fn status(&self, spec: &ServiceSpec) -> Result<()>;
}

/// Select the platform [`ServiceManager`] for the chosen scope.
///
/// The `System` scope is part of the contract but unimplemented in this pass;
/// each platform manager rejects it from its adapter methods. The compile-time
/// `cfg(target_os)` selection means only the current platform's manager is
/// built into the binary.
#[must_use]
pub fn manager_for(scope: ServiceScope) -> Box<dyn ServiceManager> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::LaunchdManager::new(scope))
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::SystemdManager::new(scope))
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::ScheduledTaskManager::new(scope))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Box::new(unsupported::UnsupportedManager::new(scope))
    }
}

/// The action a `cascade service` subcommand performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    /// Write the service definition and register it.
    Install,
    /// Deregister the service and remove its definition.
    Uninstall,
    /// Start the registered service.
    Start,
    /// Stop the registered service.
    Stop,
    /// Report the service's registration and run state.
    Status,
}

/// What the operator asked for on the command line: an explicit scope flag, or
/// nothing (leave it to inference).
///
/// The two booleans on the `service` subcommand (`--user` / `--system`) are
/// mutually exclusive at the clap layer, so only these three states are
/// reachable. Modelling the request explicitly — rather than threading two
/// bools through the resolver — keeps [`resolve_scope`] a total function over a
/// closed set of inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeRequest {
    /// `--user` was given: force the per-user scope.
    User,
    /// `--system` was given: force the machine-wide scope.
    System,
    /// No scope flag: infer from the session.
    Infer,
}

impl ScopeRequest {
    /// Build the request from the two mutually-exclusive clap flags.
    #[must_use]
    pub const fn from_flags(user: bool, system: bool) -> Self {
        match (user, system) {
            (true, _) => Self::User,
            (_, true) => Self::System,
            (false, false) => Self::Infer,
        }
    }
}

/// The session the daemon-installer is running in, as far as scope inference
/// cares.
///
/// Two independent axes: whether the invocation is attached to a terminal (so a
/// prompt could be shown and answered), and whether there is an interactive GUI
/// desktop session for the current user (so a per-user agent would have a login
/// context to run in). Headless boxes — CI, SSH without a display, system boot
/// — have no desktop session, which is the signal to prefer the machine-wide
/// scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// Standard input is a terminal, so a TTY prompt can be shown.
    pub interactive: bool,
    /// There is an interactive GUI desktop session for the current user.
    pub gui_desktop: bool,
}

impl Session {
    /// Probe the real environment for the current session.
    ///
    /// Interactivity is whether stdin is a terminal. The GUI-desktop signal is
    /// platform-specific: a windowing-system handle on Linux, the absence of a
    /// remote-shell marker on macOS, and always true on Windows (a logon task
    /// is the only scope this pass installs).
    #[must_use]
    pub fn probe() -> Self {
        Self {
            interactive: std::io::stdin().is_terminal(),
            gui_desktop: probe_gui_desktop(),
        }
    }
}

/// Detect an interactive GUI desktop session for the current user.
///
/// Linux: a session that owns a display has `DISPLAY` (X11) or
/// `WAYLAND_DISPLAY` (Wayland) set; a headless login (SSH, console, system
/// service) has neither.
#[cfg(target_os = "linux")]
fn probe_gui_desktop() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// Detect an interactive GUI desktop session for the current user.
///
/// macOS: a locally logged-in user is in the Aqua GUI session by default. The
/// signal that we are *not* — a headless or remote invocation — is an SSH
/// connection marker; an `ssh` login sets `SSH_CONNECTION` (and `SSH_TTY`).
#[cfg(target_os = "macos")]
fn probe_gui_desktop() -> bool {
    std::env::var_os("SSH_CONNECTION").is_none() && std::env::var_os("SSH_TTY").is_none()
}

/// Detect an interactive GUI desktop session for the current user.
///
/// Windows: the only scope this pass installs is a per-user logon Scheduled
/// Task, which always runs in the user's session, so the GUI-desktop axis is
/// not a discriminator here.
#[cfg(target_os = "windows")]
const fn probe_gui_desktop() -> bool {
    true
}

/// Detect an interactive GUI desktop session for the current user.
///
/// On targets without a platform probe there is no desktop session to speak
/// of, so inference treats the host as headless.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const fn probe_gui_desktop() -> bool {
    false
}

/// The scope chosen for an invocation, with the reason it was chosen.
///
/// The reason is printed so the operator always sees both *what* scope is being
/// used and *why* — the requirement that the chosen scope is never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeDecision {
    scope: ServiceScope,
    reason: String,
}

/// Resolve the install scope from the request and the session.
///
/// The order is fixed: an explicit flag always wins; otherwise the session is
/// inferred — an interactive GUI desktop chooses the per-user scope, a headless
/// or sessionless host chooses the machine-wide scope. The one place a prompt
/// is warranted is the boundary where a person at a real desktop terminal has
/// not said which scope they want: there `prompt` is consulted (it returns the
/// chosen scope), defaulting to per-user. A non-interactive invocation never
/// blocks — it takes the deterministic inference.
///
/// `prompt` is injected so the decision is a pure function of its inputs and is
/// unit-tested on every host without touching a real terminal.
fn resolve_scope(
    request: ScopeRequest,
    session: Session,
    prompt: impl FnOnce(Session) -> ServiceScope,
) -> ScopeDecision {
    match request {
        ScopeRequest::User => ScopeDecision {
            scope: ServiceScope::User,
            reason: "requested explicitly with --user".to_owned(),
        },
        ScopeRequest::System => ScopeDecision {
            scope: ServiceScope::System,
            reason: "requested explicitly with --system".to_owned(),
        },
        ScopeRequest::Infer => {
            if !session.gui_desktop {
                return ScopeDecision {
                    scope: ServiceScope::System,
                    reason: "inferred: no interactive GUI desktop session (headless host)"
                        .to_owned(),
                };
            }
            // A GUI desktop session leans towards the per-user scope. When the
            // operator is also at a terminal that can answer, the per-user vs
            // machine-wide trade-off is theirs to confirm; otherwise the
            // deterministic per-user inference stands without blocking.
            if session.interactive {
                let chosen = prompt(session);
                ScopeDecision {
                    scope: chosen,
                    reason: format!(
                        "chosen at the interactive desktop prompt (default per-user): {}",
                        chosen.label()
                    ),
                }
            } else {
                ScopeDecision {
                    scope: ServiceScope::User,
                    reason: "inferred: interactive GUI desktop session, non-interactive invocation"
                        .to_owned(),
                }
            }
        }
    }
}

/// Prompt the operator at the desktop boundary for the install scope.
///
/// Shown only when there is both a GUI desktop session and a terminal to answer
/// on. It states the per-user vs machine-wide trade-off and defaults to the
/// per-user scope (the safe, no-elevation choice) on an empty answer or any
/// read error — a read failure must not block or silently escalate.
fn prompt_desktop_scope(_session: Session) -> ServiceScope {
    use std::io::Write as _;

    print!(
        "You are at an interactive desktop session. Install Cascade as a per-user \
         service (no administrator rights), or machine-wide (requires elevation, \
         not yet implemented)?\n  [U]ser (default) / [s]ystem: "
    );
    // A failed flush must not escalate the scope; fall through to the default.
    let _ = std::io::stdout().flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return ServiceScope::User;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "s" | "system" => ServiceScope::System,
        _ => ServiceScope::User,
    }
}

/// Run a `cascade service <action>` invocation.
///
/// Resolves the install scope (explicit flag > session inference > a
/// TTY-gated desktop prompt), prints the chosen scope and the reason, builds
/// the [`ServiceSpec`] from the context, and dispatches into the platform
/// manager. The machine-wide scope is scaffolded but unbuilt; selecting it
/// reaches the platform adapter, which rejects it with a clear "not yet
/// implemented" error rather than silently doing nothing.
///
/// # Errors
///
/// Propagates any error from spec construction or the platform adapter.
pub async fn run(ctx: &CliContext, action: ServiceAction, request: ScopeRequest) -> Result<()> {
    let session = Session::probe();
    let decision = resolve_scope(request, session, prompt_desktop_scope);
    println!(
        "Using {} scope ({}).",
        decision.scope.label(),
        decision.reason
    );

    let spec = ServiceSpec::from_context(ctx)?;
    let manager = manager_for(decision.scope);
    match action {
        ServiceAction::Install => manager.install(&spec).await,
        ServiceAction::Uninstall => manager.uninstall(&spec).await,
        ServiceAction::Start => manager.start(&spec).await,
        ServiceAction::Stop => manager.stop(&spec).await,
        ServiceAction::Status => manager.status(&spec).await,
    }
}

/// Fallback manager for platforms without a service backend.
///
/// Only compiled on targets that are none of macOS / Linux / Windows; on those
/// hosts `cascade service` has no implementation and every action reports that.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod unsupported {
    use anyhow::Result;
    use async_trait::async_trait;

    use super::{ServiceManager, ServiceScope, ServiceSpec};

    pub struct UnsupportedManager;

    impl UnsupportedManager {
        #[must_use]
        pub const fn new(_scope: ServiceScope) -> Self {
            Self
        }
    }

    #[async_trait]
    impl ServiceManager for UnsupportedManager {
        async fn install(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("cascade service is not supported on this platform")
        }

        async fn uninstall(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("cascade service is not supported on this platform")
        }

        async fn start(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("cascade service is not supported on this platform")
        }

        async fn stop(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("cascade service is not supported on this platform")
        }

        async fn status(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("cascade service is not supported on this platform")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    // The platform generators return `todo!()` in the Foundation phase, so they
    // are not exercised here; the parallel phase that implements each generator
    // adds the rendering assertions alongside it. These tests cover the pieces
    // that are real now: the spec built from context and the scope labels.

    #[test]
    fn from_context_builds_a_spec_under_the_config_dir() {
        let ctx = CliContext {
            config_dir: PathBuf::from("/home/u/.config/cascade"),
            db_path: PathBuf::from("/home/u/.config/cascade/state.db"),
            pid_path: PathBuf::from("/home/u/.config/cascade/cascade.pid"),
        };
        let spec = ServiceSpec::from_context(&ctx).unwrap();
        assert_eq!(spec.label, SERVICE_LABEL);
        assert_eq!(spec.args, vec!["start".to_owned()]);
        assert_eq!(spec.working_dir, ctx.config_dir);
        assert_eq!(spec.stdout_log, ctx.config_dir.join("cascade.out.log"));
        assert_eq!(spec.stderr_log, ctx.config_dir.join("cascade.err.log"));
        assert!(spec.keep_alive);
        assert!(spec.run_at_load);
        // The program is the running test binary, not a guessed path.
        assert!(spec.program.is_absolute());
    }

    #[test]
    fn scope_labels_are_stable() {
        assert_eq!(ServiceScope::User.label(), "user");
        assert_eq!(ServiceScope::System.label(), "system");
    }

    /// A prompt that must never be called; panics if the resolver consults it.
    fn never_prompt(_session: Session) -> ServiceScope {
        panic!("the prompt must not be consulted for this case");
    }

    #[test]
    fn scope_request_maps_the_clap_flags() {
        assert_eq!(ScopeRequest::from_flags(true, false), ScopeRequest::User);
        assert_eq!(ScopeRequest::from_flags(false, true), ScopeRequest::System);
        assert_eq!(ScopeRequest::from_flags(false, false), ScopeRequest::Infer);
    }

    #[test]
    fn explicit_user_flag_wins_over_any_session() {
        for session in [
            Session {
                interactive: true,
                gui_desktop: true,
            },
            Session {
                interactive: false,
                gui_desktop: false,
            },
        ] {
            let decision = resolve_scope(ScopeRequest::User, session, never_prompt);
            assert_eq!(decision.scope, ServiceScope::User);
        }
    }

    #[test]
    fn explicit_system_flag_wins_over_any_session() {
        for session in [
            Session {
                interactive: true,
                gui_desktop: true,
            },
            Session {
                interactive: false,
                gui_desktop: false,
            },
        ] {
            let decision = resolve_scope(ScopeRequest::System, session, never_prompt);
            assert_eq!(decision.scope, ServiceScope::System);
        }
    }

    #[test]
    fn headless_session_infers_system_without_prompting() {
        let session = Session {
            interactive: false,
            gui_desktop: false,
        };
        let decision = resolve_scope(ScopeRequest::Infer, session, never_prompt);
        assert_eq!(decision.scope, ServiceScope::System);
        assert!(decision.reason.contains("headless"));
    }

    #[test]
    fn headless_but_interactive_still_infers_system() {
        // A terminal with no GUI desktop session — e.g. SSH into a server — is
        // headless; the absent desktop decides before interactivity is weighed,
        // so no prompt is shown.
        let session = Session {
            interactive: true,
            gui_desktop: false,
        };
        let decision = resolve_scope(ScopeRequest::Infer, session, never_prompt);
        assert_eq!(decision.scope, ServiceScope::System);
    }

    #[test]
    fn desktop_non_interactive_infers_user_without_prompting() {
        let session = Session {
            interactive: false,
            gui_desktop: true,
        };
        let decision = resolve_scope(ScopeRequest::Infer, session, never_prompt);
        assert_eq!(decision.scope, ServiceScope::User);
        assert!(decision.reason.contains("non-interactive"));
    }

    #[test]
    fn desktop_interactive_consults_the_prompt() {
        let session = Session {
            interactive: true,
            gui_desktop: true,
        };
        // The prompt picks System this time; the resolver honours it.
        let decision = resolve_scope(ScopeRequest::Infer, session, |_| ServiceScope::System);
        assert_eq!(decision.scope, ServiceScope::System);
        // And per-user when the prompt defaults.
        let decision = resolve_scope(ScopeRequest::Infer, session, |_| ServiceScope::User);
        assert_eq!(decision.scope, ServiceScope::User);
    }
}
