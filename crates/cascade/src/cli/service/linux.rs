//! Linux service backend: a per-user systemd `--user` unit.
//!
//! The unit is written to `~/.config/systemd/user/<label>.service` and managed
//! with `systemctl --user` — entirely user-scoped, so no administrator rights
//! are needed. The unit generator `generate` is pure string work and is
//! compiled (and unit-tested) on every host; only the `SystemdManager`
//! lifecycle adapter that touches the filesystem and runs `systemctl` is
//! Linux-gated.
//!
//! Running without an active login session is an optional extra, not a
//! requirement: a user service only runs while the user has a session unless
//! lingering is enabled with `loginctl enable-linger <user>`. Cascade does not
//! enable it — that is a deliberate, separately-elevated choice for the
//! operator to make — but the install output points at it so a headless
//! always-on setup is one documented command away.

#[cfg(any(target_os = "linux", test))]
use super::ServiceSpec;

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

/// Render the systemd `--user` unit for `spec`.
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
/// decides whether an `[Install]` section wires the unit into
/// `default.target`, so `systemctl --user enable` starts it at logon.
#[cfg(any(target_os = "linux", test))]
#[must_use]
pub fn generate(spec: &ServiceSpec) -> String {
    let mut exec_start = quote_exec_token(&spec.program.to_string_lossy());
    for arg in &spec.args {
        exec_start.push(' ');
        exec_start.push_str(&quote_exec_token(arg));
    }

    let restart = if spec.keep_alive { "on-failure" } else { "no" };

    let install_section = if spec.run_at_load {
        "\n[Install]\nWantedBy=default.target\n"
    } else {
        ""
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
    use std::path::PathBuf;

    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use tokio::process::Command;

    use super::generate;
    use crate::cli::service::{ServiceManager, ServiceScope, ServiceSpec};

    /// Linux systemd `--user` unit manager.
    pub struct SystemdManager {
        scope: ServiceScope,
    }

    impl SystemdManager {
        /// Construct a manager for the given scope.
        #[must_use]
        pub const fn new(scope: ServiceScope) -> Self {
            Self { scope }
        }

        /// Reject the scaffolded `System` scope; only `User` is implemented.
        fn require_user_scope(&self) -> Result<()> {
            match self.scope {
                ServiceScope::User => Ok(()),
                ServiceScope::System => anyhow::bail!(
                    "system-scope Linux services are not yet implemented; \
                     use --user (the no-admin per-user systemd unit)"
                ),
            }
        }

        /// The `~/.config/systemd/user` directory the unit lives in.
        ///
        /// Resolved through `dirs::config_dir`, which honours `XDG_CONFIG_HOME`
        /// and falls back to `~/.config`, so the unit lands wherever the user's
        /// systemd `--user` instance looks for it.
        fn unit_dir() -> Result<PathBuf> {
            let config = dirs::config_dir()
                .context("could not resolve the user config directory for the systemd unit")?;
            Ok(config.join("systemd").join("user"))
        }

        /// The full path to this service's unit file.
        fn unit_path(spec: &ServiceSpec) -> Result<PathBuf> {
            Ok(Self::unit_dir()?.join(format!("{}.service", spec.label)))
        }
    }

    /// Run a `systemctl --user` invocation and fail loudly on a non-zero exit.
    ///
    /// stdout and stderr are inherited so the user sees systemd's own output —
    /// the point of `status` in particular — and a non-zero exit is surfaced as
    /// an error rather than being silently swallowed.
    async fn systemctl<I, S>(args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let status = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()
            .await
            .context("failed to run systemctl --user; is systemd available?")?;
        if !status.success() {
            anyhow::bail!("systemctl --user exited with status {status}");
        }
        Ok(())
    }

    #[async_trait]
    impl ServiceManager for SystemdManager {
        async fn install(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_user_scope()?;

            let dir = Self::unit_dir()?;
            tokio::fs::create_dir_all(&dir)
                .await
                .with_context(|| format!("creating systemd unit directory {}", dir.display()))?;

            let path = Self::unit_path(spec)?;
            let unit = generate(spec);
            tokio::fs::write(&path, unit)
                .await
                .with_context(|| format!("writing systemd unit {}", path.display()))?;

            systemctl(["daemon-reload"]).await?;
            systemctl(["enable", "--now", &spec.label]).await?;

            println!("Installed {} as a systemd --user service.", spec.label);
            println!("Unit written to {}.", path.display());
            println!(
                "To keep it running without an active login session, run: \
                 loginctl enable-linger {}",
                whoami_or_user()
            );
            Ok(())
        }

        async fn uninstall(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_user_scope()?;

            systemctl(["disable", "--now", &spec.label]).await?;

            let path = Self::unit_path(spec)?;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("removing systemd unit {}", path.display()));
                }
            }

            systemctl(["daemon-reload"]).await?;

            println!("Uninstalled {}.", spec.label);
            Ok(())
        }

        async fn start(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_user_scope()?;
            systemctl(["start", &spec.label]).await
        }

        async fn stop(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_user_scope()?;
            systemctl(["stop", &spec.label]).await
        }

        async fn status(&self, spec: &ServiceSpec) -> Result<()> {
            self.require_user_scope()?;
            systemctl(["status", &spec.label]).await
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

    use super::super::{SERVICE_LABEL, ServiceSpec};
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
        let unit = generate(&sample_spec());
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
        let on = generate(&sample_spec());
        assert!(on.contains("Restart=on-failure\n"));

        let mut spec = sample_spec();
        spec.keep_alive = false;
        let off = generate(&spec);
        assert!(off.contains("Restart=no\n"));
        assert!(!off.contains("Restart=on-failure\n"));
    }

    #[test]
    fn run_at_load_controls_the_install_section() {
        let enabled = generate(&sample_spec());
        assert!(enabled.contains("[Install]\n"));
        assert!(enabled.contains("WantedBy=default.target\n"));

        let mut spec = sample_spec();
        spec.run_at_load = false;
        let disabled = generate(&spec);
        assert!(!disabled.contains("[Install]"));
        assert!(!disabled.contains("WantedBy="));
    }

    #[test]
    fn exec_start_quotes_paths_and_args_with_spaces() {
        let mut spec = sample_spec();
        spec.program = PathBuf::from("/opt/My Apps/cascade");
        spec.args = vec!["start".to_owned(), "--flag value".to_owned()];
        let unit = generate(&spec);
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
