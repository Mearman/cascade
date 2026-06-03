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
//! * a **pure generator** — [`ServiceManager::generate`] turns a [`ServiceSpec`]
//!   into the platform's service-definition text (plist / unit / task XML). It
//!   is plain string work with no OS calls, so it is unit-tested on every host
//!   regardless of `target_os`.
//! * a **platform adapter** — the `install` / `uninstall` / `start` / `stop` /
//!   `status` methods write the generated file and drive the OS register
//!   command. Only this half is `cfg(target_os)`-gated.
//!
//! The `System` scope is scaffolded — the [`ServiceScope`] enum and the
//! manager contract both admit it — but its platform backends are deferred in
//! this pass. Only the no-admin `User` scope is implemented.

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

/// A platform's service manager: a pure generator plus the OS-facing lifecycle
/// adapter.
///
/// The generator ([`generate`](ServiceManager::generate)) is host-independent
/// string work. The lifecycle methods are the `cfg(target_os)`-gated adapter
/// that writes the generated definition and drives the OS register / control
/// commands.
#[async_trait]
pub trait ServiceManager: Send + Sync {
    /// Render the platform service-definition text for `spec`.
    ///
    /// Pure: no filesystem or process side effects, so it is exercised on every
    /// host in unit tests.
    ///
    /// The consumer is the `install` adapter, which writes the rendered
    /// definition before registering it with the OS. That call site lands in
    /// the parallel phase that implements `install`; until then the method has
    /// no non-test caller, so its dead-code denial is allowed here for the
    /// Foundation skeleton with that explicit justification.
    #[allow(dead_code)]
    fn generate(&self, spec: &ServiceSpec) -> String;

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

/// Run a `cascade service <action>` invocation.
///
/// Resolves the scope (a stub defaulting to [`ServiceScope::User`] until the
/// Integrate phase implements the real inference), prints the chosen scope and
/// why, builds the [`ServiceSpec`] from the context, and dispatches into the
/// platform manager.
///
/// # Errors
///
/// Propagates any error from spec construction or the platform adapter.
pub async fn run(ctx: &CliContext, action: ServiceAction, scope: ServiceScope) -> Result<()> {
    // Scope selection / inference is a stub for the Foundation phase; the
    // Integrate phase replaces this with flag > inference > prompt resolution.
    println!(
        "Using {} scope (default; scope inference not yet implemented).",
        scope.label()
    );

    let spec = ServiceSpec::from_context(ctx)?;
    let manager = manager_for(scope);
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
        fn generate(&self, _spec: &ServiceSpec) -> String {
            String::new()
        }

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
}
