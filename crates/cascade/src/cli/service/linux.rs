//! Linux service backend: a per-user systemd `--user` unit.
//!
//! The unit is written to `~/.config/systemd/user/<label>.service` and managed
//! with `systemctl --user` — entirely user-scoped, so no administrator rights
//! are needed. The unit generator `generate` is pure string work and is
//! compiled (and unit-tested) on every host; only the `SystemdManager`
//! lifecycle adapter that touches the filesystem and runs `systemctl` is
//! Linux-gated.

#[cfg(any(target_os = "linux", test))]
use super::ServiceSpec;

/// Render the systemd `--user` unit for `spec`.
///
/// Pure string work with no side effects. It is compiled on Linux (where the
/// adapter drives it) and under `test` on every host (so the generator is
/// unit-tested regardless of `target_os`, per the design's host-testable
/// generator requirement). It returns `todo!()` for the Foundation skeleton;
/// the parallel phase implements it and adds the rendering tests that call it.
/// Until those land it has no non-test caller on non-Linux hosts, so its
/// dead-code denial is allowed here with that explicit justification.
#[cfg(any(target_os = "linux", test))]
#[allow(dead_code)]
#[must_use]
pub fn generate(_spec: &ServiceSpec) -> String {
    todo!("render the systemd --user unit from the spec")
}

#[cfg(target_os = "linux")]
pub use adapter::SystemdManager;

#[cfg(target_os = "linux")]
mod adapter {
    use anyhow::Result;
    use async_trait::async_trait;

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
    }

    #[async_trait]
    impl ServiceManager for SystemdManager {
        fn generate(&self, spec: &ServiceSpec) -> String {
            generate(spec)
        }

        async fn install(&self, _spec: &ServiceSpec) -> Result<()> {
            let _ = self.scope;
            anyhow::bail!("Linux service install is not yet implemented")
        }

        async fn uninstall(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Linux service uninstall is not yet implemented")
        }

        async fn start(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Linux service start is not yet implemented")
        }

        async fn stop(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Linux service stop is not yet implemented")
        }

        async fn status(&self, _spec: &ServiceSpec) -> Result<()> {
            anyhow::bail!("Linux service status is not yet implemented")
        }
    }
}
