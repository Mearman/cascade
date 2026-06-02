//! CLI implementations for the manager-side management plane: administering a
//! remote node by device id.
//!
//! `cascade remote <device-id> <subcommand>` resolves the target through the
//! configured P2P backend's discovery sources, opens an authenticated session
//! over the connectivity ladder, sends a
//! [`ManageRequest`](cascade_p2p::protocol::BepMessage::ManageRequest), and
//! renders the [`ManageResponse`](cascade_p2p::protocol::BepMessage::ManageResponse).
//!
//! The transport is the P2P backend's own discovery + connection plumbing —
//! [`cascade_backend_p2p::P2pBackend::manage_remote`] — so a manager never opens
//! a parallel transport. A grant the target has not conferred surfaces as a
//! typed authorisation denial from the node, distinct from a transport failure.

use anyhow::{Context as _, Result};
use cascade_backend_p2p::P2pBackend;
use cascade_p2p::protocol::{ManageCommand, ManageErrorKind, ManageResult, ManageScope};

use super::CliContext;
use super::init::CascadeConfig;

/// A remote-administration subcommand, parsed from the clap command tree into
/// the wire [`ManageCommand`] plus the [`ManageScope`] it targets.
///
/// Kept as a CLI-side enum (rather than threading clap's types into the
/// transport) so the mapping from user-facing verbs to wire frames lives in one
/// place. `cache warm` maps to a recursive pin — warming a path is pinning it so
/// the matching files download — mirroring the local `cache warm` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCommand {
    /// Read the node's status snapshot.
    Status,
    /// Pin a path, keeping matching files offline on the node.
    Pin {
        /// The path to pin.
        path: String,
    },
    /// Remove a pin rule from the node.
    Unpin {
        /// The path whose pin rule to remove.
        path: String,
    },
    /// Run one cache eviction sweep on the node.
    CacheEvict,
    /// Warm a path on the node by pinning it so the files download.
    CacheWarm {
        /// The path to warm.
        path: String,
    },
}

impl RemoteCommand {
    /// The wire [`ManageCommand`] this subcommand sends.
    #[must_use]
    pub fn to_wire(&self) -> ManageCommand {
        match self {
            Self::Status => ManageCommand::StatusRead,
            Self::Pin { path } | Self::CacheWarm { path } => ManageCommand::Pin {
                path_glob: path.clone(),
                recursive: true,
            },
            Self::Unpin { path } => ManageCommand::Unpin {
                path_glob: path.clone(),
            },
            Self::CacheEvict => ManageCommand::CacheEvict,
        }
    }

    /// The [`ManageScope`] the request advertises.
    ///
    /// A path-bearing command advertises the path itself as a folder scope; a
    /// node-wide command ([`Self::Status`], [`Self::CacheEvict`]) advertises
    /// [`ManageScope::Node`]. The managed node independently re-derives the
    /// scope the command's payload actually touches and authorises over both,
    /// so a path advertised here cannot widen what the command may do — it is a
    /// best-effort declaration the node cross-checks, not a source of authority.
    #[must_use]
    pub fn wire_scope(&self) -> ManageScope {
        match self {
            Self::Status | Self::CacheEvict => ManageScope::Node,
            Self::Pin { path } | Self::Unpin { path } | Self::CacheWarm { path } => {
                ManageScope::Folder { path: path.clone() }
            }
        }
    }
}

/// Resolve and open the first configured P2P backend, returning the typed
/// [`P2pBackend`] so its manager-side entry point is in reach.
///
/// The management plane rides the P2P transport, so a node with no P2P backend
/// cannot administer a remote node — the command fails loudly rather than
/// inventing a transport.
fn open_p2p_backend(ctx: &CliContext) -> Result<P2pBackend> {
    let main_config_path = ctx.config_dir.join("config.toml");
    if !main_config_path.exists() {
        anyhow::bail!(
            "no config.toml at {} — run `cascade init` before administering a remote node",
            main_config_path.display()
        );
    }
    let raw = std::fs::read_to_string(&main_config_path)
        .with_context(|| format!("reading {}", main_config_path.display()))?;
    let main_config: CascadeConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", main_config_path.display()))?;

    let p2p_name = main_config
        .backends
        .iter()
        .find_map(|(name, value)| {
            value
                .get("type")
                .and_then(toml::Value::as_str)
                .filter(|t| *t == "p2p")
                .map(|_| name.clone())
        })
        .context(
            "no P2P backend configured — remote administration rides the P2P transport. \
             Add one with `cascade backend-add p2p`.",
        )?;

    let backend_config_path = ctx.config_dir.join(format!("{p2p_name}.toml"));
    let backend_raw = std::fs::read_to_string(&backend_config_path)
        .with_context(|| format!("reading {}", backend_config_path.display()))?;
    let backend_config: toml::Value = toml::from_str(&backend_raw)
        .with_context(|| format!("parsing {}", backend_config_path.display()))?;
    cascade_backend_p2p::open_from_config(&backend_config)
        .context("opening P2P backend for remote administration")
}

/// `cascade remote <device-id> <subcommand>`.
///
/// Drives the management round-trip: open the P2P backend, send the command to
/// `device_id` over the connectivity ladder, and render the node's reply. An
/// authorisation denial is reported as such and the process exits with a
/// non-zero status via the returned `Err`, distinguishing "the node refused
/// you" from "the command ran and failed".
pub async fn run(ctx: &CliContext, device_id: &str, command: RemoteCommand) -> Result<()> {
    if device_id.trim().is_empty() {
        anyhow::bail!("remote requires a non-empty device id");
    }
    let backend = open_p2p_backend(ctx)?;
    let result = backend
        .manage_remote(device_id, command.to_wire(), command.wire_scope())
        .await
        .with_context(|| format!("administering remote node {device_id}"))?;
    render(device_id, &result)
}

/// Render a [`ManageResult`] to stdout, returning an `Err` for any non-`Ok`
/// outcome so the CLI exits non-zero on a denial or a failed command.
fn render(device_id: &str, result: &ManageResult) -> Result<()> {
    match result {
        ManageResult::Ok { summary } => {
            println!("{summary}");
            Ok(())
        }
        ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message,
        } => anyhow::bail!("node {device_id} refused the command (unauthorised): {message}"),
        ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message,
        } => anyhow::bail!("command on node {device_id} failed: {message}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn status_maps_to_status_read_over_node() {
        let cmd = RemoteCommand::Status;
        assert_eq!(cmd.to_wire(), ManageCommand::StatusRead);
        assert_eq!(cmd.wire_scope(), ManageScope::Node);
    }

    #[test]
    fn pin_maps_to_recursive_pin_over_its_folder() {
        let cmd = RemoteCommand::Pin {
            path: "/work/reports".to_owned(),
        };
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::Pin {
                path_glob: "/work/reports".to_owned(),
                recursive: true,
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/work/reports".to_owned(),
            }
        );
    }

    #[test]
    fn unpin_maps_to_unpin() {
        let cmd = RemoteCommand::Unpin {
            path: "/work".to_owned(),
        };
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::Unpin {
                path_glob: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn cache_evict_maps_to_cache_evict_over_node() {
        let cmd = RemoteCommand::CacheEvict;
        assert_eq!(cmd.to_wire(), ManageCommand::CacheEvict);
        assert_eq!(cmd.wire_scope(), ManageScope::Node);
    }

    #[test]
    fn cache_warm_maps_to_recursive_pin() {
        // Warming a path is pinning it so the files download — the same wire
        // command the local `cache warm` produces.
        let cmd = RemoteCommand::CacheWarm {
            path: "/media".to_owned(),
        };
        assert_eq!(
            cmd.to_wire(),
            ManageCommand::Pin {
                path_glob: "/media".to_owned(),
                recursive: true,
            }
        );
        assert_eq!(
            cmd.wire_scope(),
            ManageScope::Folder {
                path: "/media".to_owned(),
            }
        );
    }

    #[test]
    fn render_ok_prints_summary() {
        let result = ManageResult::Ok {
            summary: "all good".to_owned(),
        };
        assert!(render("PEER", &result).is_ok());
    }

    #[test]
    fn render_unauthorised_is_error() {
        let result = ManageResult::Err {
            kind: ManageErrorKind::Unauthorised,
            message: "no grant".to_owned(),
        };
        let err = render("PEER", &result).unwrap_err();
        assert!(format!("{err:#}").contains("unauthorised"));
    }

    #[test]
    fn render_failed_is_error() {
        let result = ManageResult::Err {
            kind: ManageErrorKind::Failed,
            message: "disk full".to_owned(),
        };
        let err = render("PEER", &result).unwrap_err();
        assert!(format!("{err:#}").contains("failed"));
    }
}
