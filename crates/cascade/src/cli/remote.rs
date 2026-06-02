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

use std::path::Path;

use anyhow::{Context as _, Result};
use cascade_backend_p2p::P2pBackend;
use cascade_p2p::protocol::{
    ManageCommand, ManageConfigFormat, ManageErrorKind, ManageGrant, ManageResult, ManageScope,
};

use super::CliContext;
use super::init::CascadeConfig;

/// The wildcard scope token an operator may pass to mean "node-wide" — every
/// path on the node. Mirrors the local `grant add --scope *` spelling so the
/// manager and managed sides accept the same vocabulary.
const SCOPE_WILDCARD: &str = "*";

/// The leading path component used to confine a glob, or root when the glob
/// has no fixed prefix. The managed node re-derives the same prefix from the
/// command payload; advertising it here keeps the wire scope aligned with what
/// the node authorises over, but the node's value is the one that binds.
const ROOT_SCOPE: &str = "/";

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
    /// Push a `.cascade` config fragment to merge into the node's rule set,
    /// rooted at `folder`.
    ConfigPush {
        /// The folder the fragment applies to — the scope the push targets.
        folder: String,
        /// The serialisation format of `body`, derived from the source file's
        /// extension.
        format: ManageConfigFormat,
        /// The raw config fragment.
        body: String,
    },
    /// Set a lifecycle policy on the node over a path glob.
    PolicySet {
        /// The path glob the policy applies to — also the scope it targets.
        path_glob: String,
        /// Maximum file age before eviction, in seconds. Absent leaves the
        /// dimension unbounded.
        max_age_secs: Option<i64>,
        /// Maximum file size before eviction, in bytes. Absent leaves the
        /// dimension unbounded.
        max_file_size: Option<i64>,
        /// Priority — higher wins when policies overlap.
        priority: i32,
    },
    /// Register a backend on the node, mounted at `mount_path`.
    BackendAdd {
        /// The backend name (its identifier and config file stem).
        name: String,
        /// The backend type (`gdrive`, `s3`, `p2p`, …).
        backend_type: String,
        /// The VFS mount path the backend is mounted at — the scope this
        /// command targets.
        mount_path: String,
        /// The backend's TOML config fragment, as a literal TOML document.
        config_toml: String,
    },
    /// Remove a registered backend by name.
    BackendRemove {
        /// The backend name to remove.
        name: String,
        /// The VFS mount path the backend occupied — the scope this command
        /// targets.
        mount_path: String,
    },
    /// Restart the node's background workers, confined to a folder scope.
    Restart {
        /// The folder scope the dangerous `lifecycle:control` capability is
        /// authorised over. A dangerous capability is never satisfied by a
        /// node-wide grant, so this names an explicit folder.
        scope: String,
    },
    /// Stop the node's background workers, confined to a folder scope.
    Stop {
        /// The folder scope the dangerous `lifecycle:control` capability is
        /// authorised over.
        scope: String,
    },
    /// Delegate a grant to another device. Advertises `grant:admin`; the node
    /// enforces the subset rule, refusing any attempt to escalate beyond the
    /// caller's own authority.
    GrantAdd {
        /// The device the grant authorises, by device ID.
        grantee: String,
        /// The capability conferred, in its colon-delimited wire form.
        capability: String,
        /// The scope the capability applies over, as a folder path or the
        /// wildcard token.
        scope: String,
        /// When the grant expires, as an RFC 3339 timestamp. Absent means
        /// never.
        expires: Option<String>,
    },
    /// Revoke a grant by its row id, advertising the folder scope the caller
    /// holds `grant:admin` over. The node re-resolves the revoked grant's real
    /// stored scope and authorises over that, so this advertised scope cannot
    /// widen the revocation.
    GrantRevoke {
        /// The row id of the grant to revoke (as shown by the node's
        /// `grant list`).
        grant_id: i64,
        /// The folder scope the caller's `grant:admin` grant covers.
        scope: String,
    },
}

impl RemoteCommand {
    /// The wire [`ManageCommand`] this subcommand sends.
    ///
    /// `cache warm` maps to a recursive pin — the same wire command the local
    /// `cache warm` produces — rather than a [`ManageCommand::CacheWarm`], so a
    /// warmed path is kept offline by a pin rule. Every other verb maps to its
    /// matching `ManageCommand` variant one-to-one.
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
            Self::ConfigPush {
                folder,
                format,
                body,
            } => ManageCommand::ConfigPush {
                format: *format,
                folder: folder.clone(),
                body: body.clone(),
            },
            Self::PolicySet {
                path_glob,
                max_age_secs,
                max_file_size,
                priority,
            } => ManageCommand::PolicySet {
                path_glob: path_glob.clone(),
                max_age_secs: *max_age_secs,
                max_file_size: *max_file_size,
                priority: *priority,
            },
            Self::BackendAdd {
                name,
                backend_type,
                mount_path,
                config_toml,
            } => ManageCommand::BackendAdd {
                name: name.clone(),
                backend_type: backend_type.clone(),
                mount_path: mount_path.clone(),
                config_toml: config_toml.clone(),
            },
            Self::BackendRemove { name, mount_path } => ManageCommand::BackendRemove {
                name: name.clone(),
                mount_path: mount_path.clone(),
            },
            Self::Restart { .. } => ManageCommand::Restart,
            Self::Stop { .. } => ManageCommand::Stop,
            Self::GrantAdd {
                grantee,
                capability,
                scope,
                expires,
            } => ManageCommand::GrantAdd {
                grant: ManageGrant {
                    grantee: grantee.clone(),
                    capability: capability.clone(),
                    scope: scope_from_arg(scope),
                    expires: expires.clone(),
                },
            },
            Self::GrantRevoke { grant_id, scope } => ManageCommand::GrantRevoke {
                grant_id: *grant_id,
                scope: scope_from_arg(scope),
            },
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
    ///
    /// The dangerous-capability commands ([`Self::Restart`], [`Self::Stop`],
    /// [`Self::BackendAdd`], [`Self::BackendRemove`], [`Self::GrantAdd`],
    /// [`Self::GrantRevoke`]) advertise an explicit folder scope: a dangerous
    /// capability is never satisfied by a node-wide grant, so a node-wide scope
    /// could never authorise them.
    #[must_use]
    pub fn wire_scope(&self) -> ManageScope {
        match self {
            Self::Status | Self::CacheEvict => ManageScope::Node,
            Self::Pin { path } | Self::Unpin { path } | Self::CacheWarm { path } => {
                ManageScope::Folder { path: path.clone() }
            }
            Self::ConfigPush { folder, .. } => ManageScope::Folder {
                path: folder.clone(),
            },
            Self::PolicySet { path_glob, .. } => ManageScope::Folder {
                path: path_glob.clone(),
            },
            Self::BackendAdd { mount_path, .. } | Self::BackendRemove { mount_path, .. } => {
                ManageScope::Folder {
                    path: mount_path.clone(),
                }
            }
            Self::Restart { scope }
            | Self::Stop { scope }
            | Self::GrantAdd { scope, .. }
            | Self::GrantRevoke { scope, .. } => scope_from_arg(scope),
        }
    }
}

/// Map a `--scope` argument string to a wire [`ManageScope`].
///
/// The wildcard token [`SCOPE_WILDCARD`] maps to [`ManageScope::Node`]; any
/// other value is a folder path prefix. Mirrors the local `grant add` scope
/// parsing so the two sides share one spelling.
#[must_use]
fn scope_from_arg(raw: &str) -> ManageScope {
    if raw == SCOPE_WILDCARD {
        ManageScope::Node
    } else {
        ManageScope::Folder {
            path: raw.to_owned(),
        }
    }
}

/// Derive the [`ManageConfigFormat`] of a `.cascade` fragment from its source
/// file's extension.
///
/// A `.toml`, `.yaml`/`.yml`, or `.json` extension selects the matching
/// structured format; anything else — including a bare `.cascade` file with no
/// extension — is treated as the gitignore-style format, matching the parser's
/// own default for an extensionless `.cascade` file.
#[must_use]
fn config_format_from_path(path: &Path) -> ManageConfigFormat {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("toml") => ManageConfigFormat::Toml,
        Some("yaml" | "yml") => ManageConfigFormat::Yaml,
        Some("json") => ManageConfigFormat::Json,
        _ => ManageConfigFormat::Gitignore,
    }
}

/// Read a `.cascade` config fragment from `path`, returning the body and the
/// format inferred from its extension.
///
/// The file must exist and be readable; a missing or unreadable file fails
/// loudly rather than pushing an empty fragment.
fn read_config_fragment(path: &Path) -> Result<(ManageConfigFormat, String)> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading config fragment {}", path.display()))?;
    Ok((config_format_from_path(path), body))
}

/// Build a [`RemoteCommand::ConfigPush`] from a local fragment file.
///
/// Reads `file` and infers its format from the extension. `scope` is the folder
/// the fragment applies to; when absent it defaults to the node root, matching
/// the engine's rooting of an unscoped fragment.
///
/// # Errors
///
/// Returns an error when the fragment file cannot be read.
pub fn config_push(file: &Path, scope: Option<&str>) -> Result<RemoteCommand> {
    let (format, body) = read_config_fragment(file)?;
    let folder = scope.unwrap_or(ROOT_SCOPE).to_owned();
    Ok(RemoteCommand::ConfigPush {
        folder,
        format,
        body,
    })
}

/// Build a [`RemoteCommand::BackendAdd`] from a local backend config file.
///
/// Reads the backend's TOML config fragment from `config` so the managed node
/// registers it exactly as the local wizard would.
///
/// # Errors
///
/// Returns an error when the config file cannot be read.
pub fn backend_add(
    name: String,
    backend_type: String,
    mount_path: String,
    config: &Path,
) -> Result<RemoteCommand> {
    let config_toml = std::fs::read_to_string(config)
        .with_context(|| format!("reading backend config {}", config.display()))?;
    Ok(RemoteCommand::BackendAdd {
        name,
        backend_type,
        mount_path,
        config_toml,
    })
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

    #[test]
    fn scope_from_arg_maps_wildcard_to_node() {
        assert_eq!(scope_from_arg("*"), ManageScope::Node);
        assert_eq!(
            scope_from_arg("/work"),
            ManageScope::Folder {
                path: "/work".to_owned(),
            }
        );
    }

    #[test]
    fn config_format_from_path_infers_by_extension() {
        assert_eq!(
            config_format_from_path(Path::new("rules.toml")),
            ManageConfigFormat::Toml
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.yaml")),
            ManageConfigFormat::Yaml
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.yml")),
            ManageConfigFormat::Yaml
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.json")),
            ManageConfigFormat::Json
        );
        // Case-insensitive.
        assert_eq!(
            config_format_from_path(Path::new("RULES.TOML")),
            ManageConfigFormat::Toml
        );
        // An extensionless `.cascade` file — and anything unrecognised — is the
        // gitignore-style default.
        assert_eq!(
            config_format_from_path(Path::new(".cascade")),
            ManageConfigFormat::Gitignore
        );
        assert_eq!(
            config_format_from_path(Path::new("rules.txt")),
            ManageConfigFormat::Gitignore
        );
    }

    #[test]
    fn config_push_reads_body_and_defaults_scope_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rules.yaml");
        std::fs::write(&file, "ignore:\n  - \"*.tmp\"\n").unwrap();

        let cmd = config_push(&file, None).unwrap();
        assert_eq!(
            cmd,
            RemoteCommand::ConfigPush {
                folder: ROOT_SCOPE.to_owned(),
                format: ManageConfigFormat::Yaml,
                body: "ignore:\n  - \"*.tmp\"\n".to_owned(),
            }
        );
    }

    #[test]
    fn config_push_missing_file_is_error() {
        assert!(config_push(Path::new("/nonexistent/rules.toml"), Some("/work")).is_err());
    }

    #[test]
    fn backend_add_reads_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("s3.toml");
        std::fs::write(&config, "type = \"s3\"\n").unwrap();

        let cmd = backend_add(
            "store".to_owned(),
            "s3".to_owned(),
            "/Archive".to_owned(),
            &config,
        )
        .unwrap();
        assert_eq!(
            cmd,
            RemoteCommand::BackendAdd {
                name: "store".to_owned(),
                backend_type: "s3".to_owned(),
                mount_path: "/Archive".to_owned(),
                config_toml: "type = \"s3\"\n".to_owned(),
            }
        );
    }

    #[test]
    fn backend_add_missing_config_is_error() {
        assert!(
            backend_add(
                "store".to_owned(),
                "s3".to_owned(),
                "/Archive".to_owned(),
                Path::new("/nonexistent/s3.toml"),
            )
            .is_err()
        );
    }
}
