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

/// Render the launchd `LaunchAgent` plist for `spec`.
///
/// Pure string work with no side effects. It is compiled on macOS (where the
/// adapter drives it) and under `test` on every host (so the generator is
/// unit-tested regardless of `target_os`, per the design's host-testable
/// generator requirement). It returns `todo!()` for the Foundation skeleton;
/// the parallel phase implements it and adds the rendering tests that call it.
/// Until those land it has no non-test caller on non-macOS hosts, so its
/// dead-code denial is allowed here with that explicit justification.
#[cfg(any(target_os = "macos", test))]
#[allow(dead_code)]
#[must_use]
pub fn generate(_spec: &ServiceSpec) -> String {
    todo!("render the launchd LaunchAgent plist from the spec")
}

#[cfg(target_os = "macos")]
pub use adapter::LaunchdManager;

#[cfg(target_os = "macos")]
mod adapter {
    use anyhow::Result;
    use async_trait::async_trait;

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
    }

    #[async_trait]
    impl ServiceManager for LaunchdManager {
        fn generate(&self, spec: &ServiceSpec) -> String {
            generate(spec)
        }

        async fn install(&self, _spec: &ServiceSpec) -> Result<()> {
            let _ = self.scope;
            anyhow::bail!("macOS service install is not yet implemented")
        }

        async fn uninstall(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("macOS service uninstall is not yet implemented")
        }

        async fn start(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("macOS service start is not yet implemented")
        }

        async fn stop(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("macOS service stop is not yet implemented")
        }

        async fn status(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("macOS service status is not yet implemented")
        }
    }
}
