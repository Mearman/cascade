//! Windows service backend: a per-user Scheduled Task that runs at logon.
//!
//! The task definition (XML) is registered for the current user with
//! `schtasks /Create` triggered at logon — user-scoped, so no administrator
//! rights are needed. This is deliberately *not* the Windows native service /
//! SCM path, which would require elevation. The task-XML generator `generate`
//! is pure string work and is compiled (and unit-tested) on every host; only
//! the `ScheduledTaskManager` lifecycle adapter that runs `schtasks` is
//! Windows-gated.

#[cfg(any(target_os = "windows", test))]
use super::ServiceSpec;

/// Render the Scheduled Task XML for `spec`.
///
/// Pure string work with no side effects. It is compiled on Windows (where the
/// adapter drives it) and under `test` on every host (so the generator is
/// unit-tested regardless of `target_os`, per the design's host-testable
/// generator requirement). It returns `todo!()` for the Foundation skeleton;
/// the parallel phase implements it and adds the rendering tests that call it.
/// Until those land it has no non-test caller on non-Windows hosts, so its
/// dead-code denial is allowed here with that explicit justification.
#[cfg(any(target_os = "windows", test))]
#[allow(dead_code)]
#[must_use]
pub fn generate(_spec: &ServiceSpec) -> String {
    todo!("render the Scheduled Task XML from the spec")
}

#[cfg(target_os = "windows")]
pub use adapter::ScheduledTaskManager;

#[cfg(target_os = "windows")]
mod adapter {
    use anyhow::Result;
    use async_trait::async_trait;

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
    }

    #[async_trait]
    impl ServiceManager for ScheduledTaskManager {
        fn generate(&self, spec: &ServiceSpec) -> String {
            generate(spec)
        }

        async fn install(&self, _spec: &ServiceSpec) -> Result<()> {
            let _ = self.scope;
            anyhow::bail!("Windows service install is not yet implemented")
        }

        async fn uninstall(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Windows service uninstall is not yet implemented")
        }

        async fn start(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Windows service start is not yet implemented")
        }

        async fn stop(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Windows service stop is not yet implemented")
        }

        async fn status(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Windows service status is not yet implemented")
        }
    }
}
