//! Linux service backend: a systemd unit, per-user or machine-wide.
//!
//! The per-user scope writes the unit to `~/.config/systemd/user/<label>.service`
//! and manages it with `systemctl --user` — entirely user-scoped, so no
//! administrator rights are needed. The system scope writes the unit to
//! `/etc/systemd/system/<label>.service` and drives the system `systemctl` (no
//! `--user`); it requires root, so a mutating system operation re-runs under
//! `sudo`. The unit generator `generate` is pure string work and is compiled
//! (and unit-tested) on every host; only the `SystemdManager` lifecycle adapter
//! that touches the filesystem and runs `systemctl` is Linux-gated.
//!
//! For a user unit, running without an active login session is an optional
//! extra, not a requirement: a user service only runs while the user has a
//! session unless lingering is enabled with `loginctl enable-linger <user>`.
//! Cascade does not enable it — that is a deliberate, separately-elevated choice
//! for the operator to make — but the install output points at it so a headless
//! always-on setup is one documented command away. A system unit runs at boot
//! already, so no linger hint applies there.

#[cfg(any(target_os = "linux", test))]
use super::{ServiceScope, ServiceSpec};

/// Quote a single token for a systemd `ExecStart` command line.
///
/// systemd splits `ExecStart` on whitespace, treats double quotes as grouping,
/// and inside double quotes honours `\"` and `\\` escapes. A token with no
/// whitespace, quotes, or backslashes needs no quoting; anything else is
/// wrapped in double quotes with `\` and `"` escaped so the path or argument
/// survives intact (e.g. an install directory containing a space).
#[cfg(any(target_os = "linux", test))]
fn quote_exec_token(token: &str) -> String {
    let needs_quoting = token.is_empty()
        || token
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\');
    if !needs_quoting {
        return token.to_owned();
    }
    let mut quoted = String::with_capacity(token.len() + 2);
    quoted.push('"');
    for c in token.chars() {
        if c == '"' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// The `WantedBy` target a unit's `[Install]` section wires into for `scope`.
///
/// The user manager and the system manager have separate target namespaces:
/// `default.target` is the user instance's boot-equivalent target reached by
/// `systemctl --user enable`, whereas `multi-user.target` is the system
/// instance's normal multi-user boot target. Emitting the wrong one leaves the
/// unit enabled against a target the active manager never reaches, so it never
/// starts at load.
#[cfg(any(target_os = "linux", test))]
const fn wanted_target(scope: ServiceScope) -> &'static str {
    match scope {
        ServiceScope::User => "default.target",
        ServiceScope::System => "multi-user.target",
    }
}

/// Render the systemd unit for `spec` in `scope`.
///
/// Pure string work with no side effects. It is compiled on Linux (where the
/// adapter drives it) and under `test` on every host (so the generator is
/// unit-tested regardless of `target_os`, per the design's host-testable
/// generator requirement).
///
/// The unit is a `Type=simple` service whose `ExecStart` is the cascade binary
/// plus its arguments, with stdout and stderr appended to the spec's log
/// files. `Restart=on-failure` honours [`ServiceSpec::keep_alive`] — when keep
/// alive is off the unit does not restart at all. [`ServiceSpec::run_at_load`]
/// decides whether an `[Install]` section wires the unit into its scope's
/// target — `default.target` for the user manager, `multi-user.target` for the
/// system manager — so the matching `systemctl enable` starts it at load.
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub fn generate(spec: &ServiceSpec, scope: ServiceScope) -> String {
    let mut exec_start = quote_exec_token(&spec.program.to_string_lossy());
    for arg in &spec.args {
        exec_start.push(' ');
        exec_start.push_str(&quote_exec_token(arg));
    }

    let restart = if spec.keep_alive { "on-failure" } else { "no" };

    let install_section = if spec.run_at_load {
        format!("\n[Install]\nWantedBy={}\n", wanted_target(scope))
    } else {
        String::new()
    };

    format!(
        "[Unit]\n\
         Description=Cascade daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         WorkingDirectory={working_dir}\n\
         StandardOutput=append:{stdout_log}\n\
         StandardError=append:{stderr_log}\n\
         Restart={restart}\n\
         RestartSec=5\n\
         {install_section}",
        working_dir = spec.working_dir.display(),
        stdout_log = spec.stdout_log.display(),
        stderr_log = spec.stderr_log.display(),
    )
}

#[cfg(target_os = "linux")]
pub use adapter::SystemdManager;

#[cfg(target_os = "linux")]
mod adapter {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use nix::unistd::Uid;
    use tokio::process::Command;

    use super::generate;
    use crate::cli::service::{ServiceManager, ServiceScope, ServiceSpec};

    /// The machine-wide unit directory the system manager reads.
    const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

    /// Linux systemd unit manager, per-user (`systemctl --user`) or machine-wide
    /// (`systemctl`).
    pub struct SystemdManager {
        scope: ServiceScope,
    }

    impl SystemdManager {
        /// Construct a manager for the given scope.
        #[must_use]
        pub const fn new(scope: ServiceScope) -> Self {
            Self { scope }
        }

        /// The directory the unit file lives in for the active scope.
        ///
        /// User: `~/.config/systemd/user`, resolved through `dirs::config_dir`,
        /// which honours `XDG_CONFIG_HOME` and falls back to `~/.config`, so the
        /// unit lands wherever the user's systemd `--user` instance looks for
        /// it. System: `/etc/systemd/system`, the machine-wide unit directory.
        fn unit_dir(&self) -> Result<PathBuf> {
            match self.scope {
                ServiceScope::User => {
                    let config = dirs::config_dir().context(
                        "could not resolve the user config directory for the systemd unit",
                    )?;
                    Ok(config.join("systemd").join("user"))
                }
                ServiceScope::System => Ok(PathBuf::from(SYSTEM_UNIT_DIR)),
            }
        }

        /// The full path to this service's unit file in the active scope.
        fn unit_path(&self, spec: &ServiceSpec) -> Result<PathBuf> {
            Ok(self.unit_dir()?.join(format!("{}.service", spec.label)))
        }

        /// Require effective root for a system-scope operation that mutates
        /// machine-wide state.
        ///
        /// The user scope never needs this — it touches only the user's own
        /// tree and drives the user manager. The system scope writes under
        /// `/etc/systemd/system` and enables units in the system manager, both
        /// of which require root, so a non-root invocation bails with an
        /// actionable hint to re-run under `sudo` rather than failing partway
        /// through with a bare permission error.
        fn require_root_for_system(&self) -> Result<()> {
            if self.scope == ServiceScope::System && !Uid::effective().is_root() {
                anyhow::bail!(
                    "installing a system-wide cascade service requires root; \
                     re-run with sudo (e.g. sudo cascade service install --system)"
                );
            }
            Ok(())
        }

        /// Run a scope-appropriate `systemctl` invocation and fail loudly on a
        /// non-zero exit.
        ///
        /// The user scope passes `--user` to drive the per-user manager; the
        /// system scope omits it to drive the system manager. stdout and stderr
        /// are inherited so the operator sees systemd's own output — the point
        /// of `status` in particular — and a non-zero exit is surfaced as an
        /// error rather than being silently swallowed.
        async fn systemctl<I, S>(&self, args: I) -> Result<()>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            let mut command = Command::new("systemctl");
            if self.scope == ServiceScope::User {
                command.arg("--user");
            }
            let status = command
                .args(args)
                .status()
                .await
                .context("failed to run systemctl; is systemd available?")?;
            if !status.success() {
                anyhow::bail!("systemctl exited with status {status}");
            }
            Ok(())
        }

        /// Print the scope-appropriate post-install guidance.
        ///
        /// A user unit only runs while the user has a session unless lingering
        /// is enabled, so it points at `loginctl enable-linger`. A system unit
        /// runs at boot already, so it states that instead of an irrelevant
        /// linger hint.
        fn print_install_hint(&self, path: &Path) {
            match self.scope {
                ServiceScope::User => {
                    println!(
                        "Installed {} as a systemd --user service.",
                        path_label(path)
                    );
                    println!("Unit written to {}.", path.display());
                    println!(
                        "To keep it running without an active login session, run: \
                         loginctl enable-linger {}",
                        whoami_or_user()
                    );
                }
                ServiceScope::System => {
                    println!(
                        "Installed {} as a system-wide systemd service.",
                        path_label(path)
                    );
                    println!("Unit written to {}.", path.display());
                    println!("It is enabled and will start automatically at boot.");
                }
            }
        }
    }

    /// The unit file's stem (the service label) for a friendly install message.
    fn path_label(path: &Path) -> String {
        path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    #[async_trait]
    impl ServiceManager for SystemdManager {
        async fn install(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_root_for_system()?;

            let dir = self.unit_dir()?;
            tokio::fs::create_dir_all(&dir)
                .await
                .with_context(|| format!("creating systemd unit directory {}", dir.display()))?;

            let path = self.unit_path(spec)?;
            let unit = generate(spec, self.scope);
            tokio::fs::write(&path, unit)
                .await
                .with_context(|| format!("writing systemd unit {}", path.display()))?;

            self.systemctl(["daemon-reload"]).await?;
            self.systemctl(["enable", "--now", &spec.label]).await?;

            self.print_install_hint(&path);
            Ok(())
        }

        async fn uninstall(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_root_for_system()?;

            self.systemctl(["disable", "--now", &spec.label]).await?;

            let path = self.unit_path(spec)?;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("removing systemd unit {}", path.display()));
                }
            }

            self.systemctl(["daemon-reload"]).await?;

            println!("Uninstalled {}.", spec.label);
            Ok(())
        }

        async fn start(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_root_for_system()?;
            self.systemctl(["start", &spec.label]).await
        }

        async fn stop(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_root_for_system()?;
            self.systemctl(["stop", &spec.label]).await
        }

        async fn status(&self, spec: &ServiceSpec) -> Result<()> {
            // `systemctl status` of a system unit reads state only; it does not
            // mutate machine-wide state and does not need root, so no root check
            // is applied here. The user scope likewise needs none.
            self.systemctl(["status", &spec.label]).await
        }
    }

    /// The current user's login name for the linger hint, or a neutral
    /// placeholder if the environment does not name it.
    fn whoami_or_user() -> String {
        std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "$USER".to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::path::PathBuf;

    use super::super::{SERVICE_LABEL, ServiceScope, ServiceSpec};
    use super::{generate, quote_exec_token};

    fn sample_spec() -> ServiceSpec {
        ServiceSpec {
            label: SERVICE_LABEL.to_owned(),
            program: PathBuf::from("/usr/local/bin/cascade"),
            args: vec!["start".to_owned()],
            stdout_log: PathBuf::from("/home/u/.config/cascade/cascade.out.log"),
            stderr_log: PathBuf::from("/home/u/.config/cascade/cascade.err.log"),
            working_dir: PathBuf::from("/home/u/.config/cascade"),
            keep_alive: true,
            run_at_load: true,
        }
    }

    #[test]
    fn renders_a_simple_service_with_the_exec_start_and_logs() {
        let unit = generate(&sample_spec(), ServiceScope::User);
        assert!(unit.contains("[Unit]\n"));
        assert!(unit.contains("[Service]\n"));
        assert!(unit.contains("Type=simple\n"));
        assert!(unit.contains("ExecStart=/usr/local/bin/cascade start\n"));
        assert!(unit.contains("WorkingDirectory=/home/u/.config/cascade\n"));
        assert!(unit.contains("StandardOutput=append:/home/u/.config/cascade/cascade.out.log\n"));
        assert!(unit.contains("StandardError=append:/home/u/.config/cascade/cascade.err.log\n"));
    }

    #[test]
    fn keep_alive_sets_restart_on_failure_and_off_disables_it() {
        let on = generate(&sample_spec(), ServiceScope::User);
        assert!(on.contains("Restart=on-failure\n"));

        let mut spec = sample_spec();
        spec.keep_alive = false;
        let off = generate(&spec, ServiceScope::User);
        assert!(off.contains("Restart=no\n"));
        assert!(!off.contains("Restart=on-failure\n"));
    }

    #[test]
    fn run_at_load_controls_the_install_section() {
        let enabled = generate(&sample_spec(), ServiceScope::User);
        assert!(enabled.contains("[Install]\n"));
        assert!(enabled.contains("WantedBy=default.target\n"));

        let mut spec = sample_spec();
        spec.run_at_load = false;
        let disabled = generate(&spec, ServiceScope::User);
        assert!(!disabled.contains("[Install]"));
        assert!(!disabled.contains("WantedBy="));
    }

    #[test]
    fn user_scope_wires_into_default_target() {
        let unit = generate(&sample_spec(), ServiceScope::User);
        assert!(unit.contains("[Install]\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
        assert!(!unit.contains("multi-user.target"));
    }

    #[test]
    fn system_scope_wires_into_multi_user_target() {
        let unit = generate(&sample_spec(), ServiceScope::System);
        assert!(unit.contains("[Install]\n"));
        assert!(unit.contains("WantedBy=multi-user.target\n"));
        assert!(!unit.contains("default.target"));
    }

    #[test]
    fn run_at_load_off_omits_install_for_both_scopes() {
        for scope in [ServiceScope::User, ServiceScope::System] {
            let mut spec = sample_spec();
            spec.run_at_load = false;
            let unit = generate(&spec, scope);
            assert!(!unit.contains("[Install]"));
            assert!(!unit.contains("WantedBy="));
        }
    }

    #[test]
    fn exec_start_quotes_paths_and_args_with_spaces() {
        let mut spec = sample_spec();
        spec.program = PathBuf::from("/opt/My Apps/cascade");
        spec.args = vec!["start".to_owned(), "--flag value".to_owned()];
        let unit = generate(&spec, ServiceScope::User);
        assert!(unit.contains("ExecStart=\"/opt/My Apps/cascade\" start \"--flag value\"\n"));
    }

    #[test]
    fn quote_exec_token_leaves_plain_tokens_unquoted() {
        assert_eq!(quote_exec_token("start"), "start");
        assert_eq!(quote_exec_token("/usr/bin/cascade"), "/usr/bin/cascade");
    }

    #[test]
    fn quote_exec_token_escapes_quotes_and_backslashes() {
        assert_eq!(quote_exec_token(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_exec_token(r"a\b"), r#""a\\b""#);
        assert_eq!(quote_exec_token(""), r#""""#);
    }
}
