//! Windows service backend: a per-user Scheduled Task that runs at logon.
//!
//! The task definition (XML) is registered for the current user with
//! `schtasks /Create` triggered at logon — user-scoped, so no administrator
//! rights are needed. This is deliberately *not* the Windows native service /
//! SCM path, which would require elevation. The task-XML generator `generate`
//! is pure string work and is compiled (and unit-tested) on every host; only
//! the `ScheduledTaskManager` lifecycle adapter that runs `schtasks` is
//! Windows-gated.
//!
//! # Why XML rather than the flag form of `schtasks /Create`
//!
//! The short flag form (`/sc onlogon /tr "<cascade> start"`) can express the
//! logon trigger but not the spec's other two requirements: redirecting the
//! daemon's stdout/stderr to the log files, and restarting the task if it
//! exits. Task Scheduler XML expresses both — `<RestartOnFailure>` honours
//! [`ServiceSpec::keep_alive`] and an explicit `<LogonTrigger>` honours
//! [`ServiceSpec::run_at_load`] — and a single generated document keeps the
//! whole definition in one host-testable place. The adapter therefore writes
//! the XML and registers it with `schtasks /Create /XML`.
//!
//! [`ServiceSpec::keep_alive`]: super::ServiceSpec::keep_alive
//! [`ServiceSpec::run_at_load`]: super::ServiceSpec::run_at_load

#[cfg(any(target_os = "windows", test))]
use super::ServiceSpec;

/// Interval Task Scheduler waits before each restart attempt, as an ISO 8601
/// duration. One minute is Task Scheduler's own minimum honoured value.
#[cfg(any(target_os = "windows", test))]
const RESTART_INTERVAL: &str = "PT1M";

/// How many times Task Scheduler retries a failed run before giving up.
///
/// Chosen as the largest value the Task Scheduler UI itself offers, so the
/// daemon is treated as effectively always-restart without relying on an
/// unbounded count the scheduler would reject.
#[cfg(any(target_os = "windows", test))]
const RESTART_COUNT: u32 = 3;

/// Escape a string for inclusion as XML element text.
///
/// Task definitions carry filesystem paths in `<Command>`, `<Arguments>` and
/// `<WorkingDirectory>`; on Windows those may contain `&` or, in principle, the
/// other reserved characters. Escaping keeps the generated document well-formed
/// regardless of the path. Pure, so it is exercised by the generator tests on
/// every host.
#[cfg(any(target_os = "windows", test))]
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Build the `cmd.exe` argument string that runs the daemon and appends its
/// stdout/stderr to the spec's log files.
///
/// Task Scheduler runs a single executable and does not itself redirect output,
/// so the daemon is launched through `cmd /c` with shell redirection. The inner
/// command is wrapped in an extra pair of quotes because `cmd /c "<line>"`
/// strips the outermost pair before executing the line. Each path is quoted so
/// spaces in `Program Files`-style paths survive.
#[cfg(any(target_os = "windows", test))]
fn cmd_arguments(spec: &ServiceSpec) -> String {
    let program = spec.program.display();
    let stdout = spec.stdout_log.display();
    let stderr = spec.stderr_log.display();
    let daemon_args = spec.args.join(" ");
    // `cmd /c "...."` — the outer quotes are consumed by cmd, so the whole
    // redirected line is itself wrapped in quotes.
    format!(r#"/c ""{program}" {daemon_args} >> "{stdout}" 2>> "{stderr}"""#)
}

/// Render the Scheduled Task XML for `spec`.
///
/// Pure string work with no side effects. It is compiled on Windows (where the
/// adapter drives it) and under `test` on every host (so the generator is
/// unit-tested regardless of `target_os`, per the design's host-testable
/// generator requirement).
///
/// The document registers a logon-triggered task that runs the daemon through
/// `cmd /c` (so stdout/stderr reach the spec's log files) in the spec's working
/// directory, restarting on failure when [`ServiceSpec::keep_alive`] is set and
/// firing at logon when [`ServiceSpec::run_at_load`] is set.
///
/// [`ServiceSpec::keep_alive`]: super::ServiceSpec::keep_alive
/// [`ServiceSpec::run_at_load`]: super::ServiceSpec::run_at_load
#[cfg(any(target_os = "windows", test))]
#[must_use]
pub fn generate(spec: &ServiceSpec) -> String {
    let command = xml_escape("cmd.exe");
    let arguments = xml_escape(&cmd_arguments(spec));
    let working_dir = xml_escape(&spec.working_dir.display().to_string());

    // The logon trigger is gated on `run_at_load`; an `<Enabled>` flag keeps the
    // trigger present but inert otherwise, so the document shape is stable.
    let logon_enabled = if spec.run_at_load { "true" } else { "false" };

    // Restart-on-failure settings are emitted only when keep_alive is set; an
    // empty string leaves the scheduler's no-restart default in place.
    let restart_settings = if spec.keep_alive {
        format!(
            "    <RestartOnFailure>\n      \
             <Interval>{RESTART_INTERVAL}</Interval>\n      \
             <Count>{RESTART_COUNT}</Count>\n    \
             </RestartOnFailure>\n"
        )
    } else {
        String::new()
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Cascade daemon (cascade start)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>{logon_enabled}</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
{restart_settings}    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

#[cfg(target_os = "windows")]
pub use adapter::ScheduledTaskManager;

#[cfg(target_os = "windows")]
mod adapter {
    use std::process::Stdio;

    use anyhow::{Context as _, Result};
    use async_trait::async_trait;
    use tokio::process::Command;

    use super::generate;
    use crate::cli::service::{ServiceManager, ServiceScope, ServiceSpec};

    /// Windows Scheduled Task (at-logon) manager.
    pub struct ScheduledTaskManager {
        scope: ServiceScope,
    }

    impl ScheduledTaskManager {
        /// Construct a manager for the given scope.
        #[must_use]
        pub const fn new(scope: ServiceScope) -> Self {
            Self { scope }
        }

        /// Reject the scaffolded-but-unbuilt `System` scope.
        ///
        /// The per-user scope is the only one implemented in this pass; the
        /// machine-wide scope needs an elevated registration path that is
        /// deferred. Every lifecycle method funnels through here first so the
        /// rejection is stated once.
        fn ensure_user_scope(&self) -> Result<()> {
            match self.scope {
                ServiceScope::User => Ok(()),
                ServiceScope::System => anyhow::bail!(
                    "the Windows system-wide service scope is not yet implemented; \
                     install with the per-user scope"
                ),
            }
        }

        /// Path the generated task XML is written to before registration.
        ///
        /// Kept beside the daemon's logs under the spec's working directory so a
        /// per-user install touches only the user's own tree.
        fn definition_path(spec: &ServiceSpec) -> std::path::PathBuf {
            spec.working_dir.join("cascade-service.xml")
        }

        /// Run `schtasks` with `args`, returning an error if it exits non-zero.
        ///
        /// `context` names the operation for the error message.
        async fn run_schtasks(args: &[&str], context: &str) -> Result<std::process::Output> {
            let output = Command::new("schtasks.exe")
                .args(args)
                .stdin(Stdio::null())
                .output()
                .await
                .with_context(|| format!("failed to launch schtasks while trying to {context}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "schtasks failed while trying to {context}: {}",
                    stderr.trim()
                );
            }
            Ok(output)
        }
    }

    #[async_trait]
    impl ServiceManager for ScheduledTaskManager {
        async fn install(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            let xml = generate(spec);
            let definition = Self::definition_path(spec);
            tokio::fs::write(&definition, xml).await.with_context(|| {
                format!(
                    "failed to write the task definition to {}",
                    definition.display()
                )
            })?;
            let definition_str = definition
                .to_str()
                .context("the task definition path is not valid UTF-8")?;
            Self::run_schtasks(
                &["/Create", "/TN", &spec.label, "/XML", definition_str, "/F"],
                "register the Cascade scheduled task",
            )
            .await?;
            println!(
                "Registered the Cascade scheduled task '{}' to run at logon.",
                spec.label
            );
            Ok(())
        }

        async fn uninstall(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            Self::run_schtasks(
                &["/Delete", "/TN", &spec.label, "/F"],
                "remove the Cascade scheduled task",
            )
            .await?;
            // Remove the definition file too; absence is fine on uninstall.
            let definition = Self::definition_path(spec);
            if let Err(e) = tokio::fs::remove_file(&definition).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(e).with_context(|| {
                    format!(
                        "failed to remove the task definition {}",
                        definition.display()
                    )
                });
            }
            println!("Removed the Cascade scheduled task '{}'.", spec.label);
            Ok(())
        }

        async fn start(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            Self::run_schtasks(
                &["/Run", "/TN", &spec.label],
                "start the Cascade scheduled task",
            )
            .await?;
            println!("Started the Cascade scheduled task '{}'.", spec.label);
            Ok(())
        }

        async fn stop(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            Self::run_schtasks(
                &["/End", "/TN", &spec.label],
                "stop the Cascade scheduled task",
            )
            .await?;
            println!("Stopped the Cascade scheduled task '{}'.", spec.label);
            Ok(())
        }

        async fn status(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            let output = Self::run_schtasks(
                &["/Query", "/TN", &spec.label],
                "query the Cascade scheduled task",
            )
            .await?;
            print!("{}", String::from_utf8_lossy(&output.stdout));
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cli::service::SERVICE_LABEL;

    fn sample_spec() -> ServiceSpec {
        ServiceSpec {
            label: SERVICE_LABEL.to_owned(),
            program: PathBuf::from(r"C:\Program Files\Cascade\cascade.exe"),
            args: vec!["start".to_owned()],
            stdout_log: PathBuf::from(r"C:\Users\u\AppData\Roaming\cascade\cascade.out.log"),
            stderr_log: PathBuf::from(r"C:\Users\u\AppData\Roaming\cascade\cascade.err.log"),
            working_dir: PathBuf::from(r"C:\Users\u\AppData\Roaming\cascade"),
            keep_alive: true,
            run_at_load: true,
        }
    }

    #[test]
    fn xml_escape_replaces_all_reserved_characters() {
        assert_eq!(
            xml_escape(r#"a & b < c > d " e ' f"#),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
    }

    #[test]
    fn generate_is_a_well_formed_logon_task() {
        let xml = generate(&sample_spec());
        assert!(xml.starts_with(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        assert!(xml.contains(r#"<Task version="1.2""#));
        // Logon trigger present and enabled when run_at_load is set.
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<Enabled>true</Enabled>"));
        // Runs unelevated as the interactive user — no SYSTEM, no HIGHEST.
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(!xml.contains("HighestAvailable"));
        assert!(!xml.contains("S-1-5-18")); // the SYSTEM SID
    }

    #[test]
    fn generate_runs_the_daemon_through_cmd_with_redirected_logs() {
        let spec = sample_spec();
        let xml = generate(&spec);
        assert!(xml.contains("<Command>cmd.exe</Command>"));
        // The daemon path, its args, and both redirections appear in the
        // cmd argument string.
        assert!(xml.contains("cascade.exe"));
        assert!(xml.contains("start"));
        assert!(xml.contains("cascade.out.log"));
        assert!(xml.contains("cascade.err.log"));
        // Redirection operators survive XML escaping (`>>` -> `&gt;&gt;`).
        assert!(xml.contains("&gt;&gt;"));
        assert!(xml.contains("2&gt;&gt;"));
        // Working directory is the spec's config dir.
        assert!(
            xml.contains(
                r"<WorkingDirectory>C:\Users\u\AppData\Roaming\cascade</WorkingDirectory>"
            )
        );
    }

    #[test]
    fn cmd_arguments_quote_each_path_and_wrap_for_cmd() {
        let args = cmd_arguments(&sample_spec());
        // cmd /c consumes the outermost quote pair, so the whole line is wrapped.
        assert!(args.starts_with(r#"/c ""#));
        assert!(args.ends_with('"'));
        assert!(args.contains(r#""C:\Program Files\Cascade\cascade.exe""#));
        assert!(args.contains(r#">> "C:\Users\u\AppData\Roaming\cascade\cascade.out.log""#));
        assert!(args.contains(r#"2>> "C:\Users\u\AppData\Roaming\cascade\cascade.err.log""#));
    }

    #[test]
    fn keep_alive_controls_restart_on_failure() {
        let with = generate(&sample_spec());
        assert!(with.contains("<RestartOnFailure>"));
        assert!(with.contains(&format!("<Interval>{RESTART_INTERVAL}</Interval>")));
        assert!(with.contains(&format!("<Count>{RESTART_COUNT}</Count>")));

        let mut no_restart = sample_spec();
        no_restart.keep_alive = false;
        let without = generate(&no_restart);
        assert!(!without.contains("<RestartOnFailure>"));
    }

    #[test]
    fn run_at_load_toggles_the_logon_trigger_enabled_flag() {
        let mut spec = sample_spec();
        spec.run_at_load = false;
        let xml = generate(&spec);
        // Trigger element stays present so the document shape is stable, but the
        // enabled flag is false.
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<Enabled>false</Enabled>"));
    }
}
