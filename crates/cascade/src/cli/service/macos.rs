//! macOS service backend: a per-user launchd `LaunchAgent`.
//!
//! The agent plist is written to `~/Library/LaunchAgents/<label>.plist` and
//! registered with `launchctl bootstrap gui/<uid>` — both user-scoped, so no
//! administrator rights are needed. The plist generator `generate` is pure
//! string work and is compiled (and unit-tested) on every host; only the
//! `LaunchdManager` lifecycle adapter that touches the filesystem and runs
//! `launchctl` is macOS-gated.

#[cfg(any(target_os = "macos", test))]
use super::ServiceSpec;

/// XML 1.0 document declaration and Apple plist DTD prologue.
///
/// Every launchd property list opens with this exact preamble; `launchctl`
/// rejects a plist whose `DOCTYPE` does not name the Apple DTD.
#[cfg(any(target_os = "macos", test))]
const PLIST_PROLOGUE: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
    "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
    "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
);

/// Escape the five XML predefined entities in text destined for element
/// content.
///
/// Paths and arguments are operator-supplied and may legitimately contain `&`,
/// `<`, or `>`; emitting them raw would produce a malformed plist that
/// `launchctl` refuses to load. `"` and `'` are not significant in element
/// text but are escaped too so the same helper is safe for attribute values.
#[cfg(any(target_os = "macos", test))]
fn xml_escape(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Render the launchd `LaunchAgent` plist for `spec`.
///
/// Pure string work with no side effects. It is compiled on macOS (where the
/// adapter drives it) and under `test` on every host (so the generator is
/// unit-tested regardless of `target_os`, per the design's host-testable
/// generator requirement).
///
/// The plist sets `Label` to the spec label, `ProgramArguments` to the
/// program path followed by its arguments (the cascade binary and `start`),
/// `RunAtLoad` and `KeepAlive` from the spec's lifecycle switches,
/// `WorkingDirectory` to the spec's working directory, and
/// `StandardOutPath` / `StandardErrorPath` to the spec's log files.
#[cfg(any(target_os = "macos", test))]
#[must_use]
pub fn generate(spec: &ServiceSpec) -> String {
    use std::fmt::Write as _;
    let program = spec.program.to_string_lossy();
    let program_arguments = std::iter::once(program.as_ref())
        .chain(spec.args.iter().map(String::as_str))
        .fold(String::new(), |mut acc, entry| {
            // Writing into a String is infallible, so the Result is discarded.
            let _ = writeln!(acc, "\t\t<string>{}</string>", xml_escape(entry));
            acc
        });

    let bool_tag = |value: bool| if value { "<true/>" } else { "<false/>" };

    format!(
        "{prologue}<plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {program_arguments}\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t{run_at_load}\n\
         \t<key>KeepAlive</key>\n\
         \t{keep_alive}\n\
         \t<key>WorkingDirectory</key>\n\
         \t<string>{working_dir}</string>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>{stdout_log}</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>{stderr_log}</string>\n\
         </dict>\n\
         </plist>\n",
        prologue = PLIST_PROLOGUE,
        label = xml_escape(&spec.label),
        program_arguments = program_arguments,
        run_at_load = bool_tag(spec.run_at_load),
        keep_alive = bool_tag(spec.keep_alive),
        working_dir = xml_escape(&spec.working_dir.to_string_lossy()),
        stdout_log = xml_escape(&spec.stdout_log.to_string_lossy()),
        stderr_log = xml_escape(&spec.stderr_log.to_string_lossy()),
    )
}

#[cfg(target_os = "macos")]
pub use adapter::LaunchdManager;

#[cfg(target_os = "macos")]
mod adapter {
    use std::path::PathBuf;
    use std::process::Output;

    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use nix::unistd::getuid;
    use tokio::process::Command;

    use super::generate;
    use crate::cli::service::{ServiceManager, ServiceScope, ServiceSpec};

    /// macOS launchd `LaunchAgent` manager.
    pub struct LaunchdManager {
        scope: ServiceScope,
    }

    impl LaunchdManager {
        /// Construct a manager for the given scope.
        #[must_use]
        pub const fn new(scope: ServiceScope) -> Self {
            Self { scope }
        }

        /// Reject the unimplemented `System` scope before any side effect.
        ///
        /// The per-user `LaunchAgent` is the only scope built in this pass; a
        /// machine-wide `LaunchDaemon` (under `/Library/LaunchDaemons`,
        /// registered in the `system` domain) needs elevation and is deferred.
        fn ensure_user_scope(&self) -> Result<()> {
            match self.scope {
                ServiceScope::User => Ok(()),
                ServiceScope::System => anyhow::bail!(
                    "the macOS system scope (LaunchDaemon) is not yet implemented; \
                     use the per-user scope, which needs no administrator rights"
                ),
            }
        }

        /// Absolute path to the `LaunchAgents` plist for this service.
        ///
        /// `~/Library/LaunchAgents/<label>.plist` — the per-user agent
        /// directory launchd reads on login, owned by and writable as the
        /// current user with no elevation.
        fn plist_path(spec: &ServiceSpec) -> Result<PathBuf> {
            let home = dirs::home_dir()
                .context("could not resolve the home directory for the LaunchAgents path")?;
            Ok(home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{}.plist", spec.label)))
        }

        /// The launchd GUI domain target for the current user, e.g. `gui/501`.
        fn gui_domain() -> String {
            format!("gui/{}", getuid().as_raw())
        }

        /// The launchd service target for this label, e.g. `gui/501/io.cascade.daemon`.
        fn service_target(spec: &ServiceSpec) -> String {
            format!("{}/{}", Self::gui_domain(), spec.label)
        }

        /// Run `launchctl` with the given arguments, returning its captured output.
        async fn launchctl(args: &[&str]) -> Result<Output> {
            Command::new("launchctl")
                .args(args)
                .output()
                .await
                .with_context(|| format!("failed to spawn launchctl {}", args.join(" ")))
        }

        /// Run `launchctl` and fail if it exits non-zero, surfacing its stderr.
        async fn launchctl_checked(args: &[&str]) -> Result<()> {
            let output = Self::launchctl(args).await?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "launchctl {} failed ({}): {}",
                args.join(" "),
                output.status,
                stderr.trim()
            )
        }
    }

    #[async_trait]
    impl ServiceManager for LaunchdManager {
        async fn install(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            let plist_path = Self::plist_path(spec)?;
            if let Some(parent) = plist_path.parent() {
                tokio::fs::create_dir_all(parent).await.with_context(|| {
                    format!(
                        "could not create the LaunchAgents directory {}",
                        parent.display()
                    )
                })?;
            }
            let plist = generate(spec);
            tokio::fs::write(&plist_path, plist)
                .await
                .with_context(|| {
                    format!(
                        "could not write the LaunchAgent plist {}",
                        plist_path.display()
                    )
                })?;

            let domain = Self::gui_domain();
            let plist_str = plist_path.to_string_lossy();
            Self::launchctl_checked(&["bootstrap", &domain, &plist_str]).await?;
            // `enable` keeps the service eligible to run after a future
            // `disable`; harmless on a fresh bootstrap and makes re-installs
            // idempotent against an earlier `stop`-driven disable.
            let target = Self::service_target(spec);
            Self::launchctl_checked(&["enable", &target]).await?;

            println!(
                "Installed and bootstrapped the LaunchAgent {} ({}).",
                spec.label,
                plist_path.display()
            );
            Ok(())
        }

        async fn uninstall(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            let plist_path = Self::plist_path(spec)?;
            let domain = Self::gui_domain();
            let plist_str = plist_path.to_string_lossy();
            // `bootout` deregisters the agent from the running launchd domain.
            // It is allowed to fail when the agent was never bootstrapped, so
            // its failure is reported but does not block removing the file —
            // the file is the durable state that must go.
            let bootout = Self::launchctl(&["bootout", &domain, &plist_str]).await?;
            if !bootout.status.success() {
                let stderr = String::from_utf8_lossy(&bootout.stderr);
                println!(
                    "launchctl bootout {} {} reported: {} (continuing to remove the plist)",
                    domain,
                    plist_str,
                    stderr.trim()
                );
            }
            match tokio::fs::remove_file(&plist_path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "could not remove the LaunchAgent plist {}",
                            plist_path.display()
                        )
                    });
                }
            }
            println!("Removed the LaunchAgent {}.", spec.label);
            Ok(())
        }

        async fn start(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            let target = Self::service_target(spec);
            // `kickstart` starts the registered service; `-k` first kills a
            // running instance so `start` is a deterministic (re)start.
            Self::launchctl_checked(&["kickstart", "-k", &target]).await?;
            println!("Started the LaunchAgent {}.", spec.label);
            Ok(())
        }

        async fn stop(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            let target = Self::service_target(spec);
            // SIGTERM is the daemon's clean-shutdown signal; the foreground
            // daemon handles SIGTERM, so this stops it gracefully.
            Self::launchctl_checked(&["kill", "SIGTERM", &target]).await?;
            println!("Sent SIGTERM to the LaunchAgent {}.", spec.label);
            Ok(())
        }

        async fn status(&self, spec: &ServiceSpec) -> Result<()> {
            self.ensure_user_scope()?;
            let target = Self::service_target(spec);
            let output = Self::launchctl(&["print", &target]).await?;
            if output.status.success() {
                let report = String::from_utf8_lossy(&output.stdout);
                print!("{report}");
                return Ok(());
            }
            // A non-zero exit from `print` means launchd does not know the
            // service — it is not installed/loaded. That is a reportable
            // state, not an error to propagate.
            println!(
                "The LaunchAgent {} is not loaded (launchctl print {} found no such service).",
                spec.label, target
            );
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cli::service::{SERVICE_LABEL, ServiceSpec};

    fn sample_spec() -> ServiceSpec {
        ServiceSpec {
            label: SERVICE_LABEL.to_owned(),
            program: PathBuf::from("/usr/local/bin/cascade"),
            args: vec!["start".to_owned()],
            stdout_log: PathBuf::from("/home/u/Library/Logs/cascade.out.log"),
            stderr_log: PathBuf::from("/home/u/Library/Logs/cascade.err.log"),
            working_dir: PathBuf::from("/home/u/.config/cascade"),
            keep_alive: true,
            run_at_load: true,
        }
    }

    #[test]
    fn xml_escape_replaces_the_predefined_entities() {
        assert_eq!(
            xml_escape("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
        assert_eq!(xml_escape("plain/path"), "plain/path");
    }

    #[test]
    fn plist_opens_with_the_xml_and_apple_dtd_prologue() {
        let plist = generate(&sample_spec());
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(plist.contains(
            "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">"
        ));
        assert!(plist.contains("<plist version=\"1.0\">"));
        assert!(plist.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn plist_carries_the_label_from_the_spec() {
        let plist = generate(&sample_spec());
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(&format!("<string>{SERVICE_LABEL}</string>")));
    }

    #[test]
    fn program_arguments_are_the_program_then_its_args_in_order() {
        let plist = generate(&sample_spec());
        let args_block = plist
            .split("<key>ProgramArguments</key>")
            .nth(1)
            .expect("ProgramArguments key present");
        let array = args_block
            .split("<array>")
            .nth(1)
            .expect("array opens")
            .split("</array>")
            .next()
            .expect("array closes");
        let program_at = array
            .find("<string>/usr/local/bin/cascade</string>")
            .expect("program path present in the array");
        let start_at = array
            .find("<string>start</string>")
            .expect("start argument present in the array");
        assert!(program_at < start_at, "program must precede its arguments");
    }

    #[test]
    fn run_at_load_and_keep_alive_reflect_the_spec_switches() {
        let mut spec = sample_spec();
        spec.run_at_load = true;
        spec.keep_alive = true;
        let plist = generate(&spec);
        let run_block = plist
            .split("<key>RunAtLoad</key>")
            .nth(1)
            .expect("RunAtLoad key present");
        assert!(run_block.trim_start().starts_with("<true/>"));
        let keep_block = plist
            .split("<key>KeepAlive</key>")
            .nth(1)
            .expect("KeepAlive key present");
        assert!(keep_block.trim_start().starts_with("<true/>"));

        spec.run_at_load = false;
        spec.keep_alive = false;
        let plist = generate(&spec);
        let run_block = plist
            .split("<key>RunAtLoad</key>")
            .nth(1)
            .expect("RunAtLoad key present");
        assert!(run_block.trim_start().starts_with("<false/>"));
        let keep_block = plist
            .split("<key>KeepAlive</key>")
            .nth(1)
            .expect("KeepAlive key present");
        assert!(keep_block.trim_start().starts_with("<false/>"));
    }

    #[test]
    fn log_paths_and_working_directory_come_from_the_spec() {
        let spec = sample_spec();
        let plist = generate(&spec);
        assert!(plist.contains("<key>StandardOutPath</key>"));
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            spec.stdout_log.to_string_lossy()
        )));
        assert!(plist.contains("<key>StandardErrorPath</key>"));
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            spec.stderr_log.to_string_lossy()
        )));
        assert!(plist.contains("<key>WorkingDirectory</key>"));
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            spec.working_dir.to_string_lossy()
        )));
    }

    #[test]
    fn special_characters_in_paths_are_escaped_in_the_plist() {
        let mut spec = sample_spec();
        spec.program = PathBuf::from("/opt/A & B/cascade");
        let plist = generate(&spec);
        assert!(plist.contains("<string>/opt/A &amp; B/cascade</string>"));
        assert!(!plist.contains("/opt/A & B/cascade"));
    }
}
