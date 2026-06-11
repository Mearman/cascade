# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.118](https://github.com/Mearman/cascade/compare/cascade-v0.1.117...cascade-v0.1.118) - 2026-06-11

### Other

- update Cargo.toml dependencies

## [0.1.117](https://github.com/Mearman/cascade/compare/cascade-v0.1.116...cascade-v0.1.117) - 2026-06-11

### Other

- update Cargo.toml dependencies

## [0.1.116](https://github.com/Mearman/cascade/compare/cascade-v0.1.115...cascade-v0.1.116) - 2026-06-11

### Other

- update Cargo.toml dependencies

## [0.1.115](https://github.com/Mearman/cascade/compare/cascade-v0.1.114...cascade-v0.1.115) - 2026-06-11

### Added

- *(presenter-fuse)* path-aware inode model with child-mount injection

## [0.1.114](https://github.com/Mearman/cascade/compare/cascade-v0.1.113...cascade-v0.1.114) - 2026-06-11

### Other

- update Cargo.toml dependencies

## [0.1.113](https://github.com/Mearman/cascade/compare/cascade-v0.1.112...cascade-v0.1.113) - 2026-06-11

### Other

- update Cargo.toml dependencies

## [0.1.112](https://github.com/Mearman/cascade/compare/cascade-v0.1.111...cascade-v0.1.112) - 2026-06-11

### Other

- update Cargo.toml dependencies

## [0.1.111](https://github.com/Mearman/cascade/compare/cascade-v0.1.110...cascade-v0.1.111) - 2026-06-11

### Added

- *(cli)* add BackendConfig.mount, deterministic rebuild_backends, consistent init/backend-add
- *(engine)* assemble mount-prefixed VFS paths in the sync runner
- *(engine)* mount backends uniformly under a neutral VFS root

### Fixed

- *(webdav)* wire the engine mount table into the presenter

## [0.1.110](https://github.com/Mearman/cascade/compare/cascade-v0.1.109...cascade-v0.1.110) - 2026-06-08

### Other

- *(engine)* cover the DataAuthority P2P data-plane access gate

## [0.1.109](https://github.com/Mearman/cascade/compare/cascade-v0.1.108...cascade-v0.1.109) - 2026-06-08

### Other

- *(cache)* cover CacheManager eviction-sweep orchestration

## [0.1.108](https://github.com/Mearman/cascade/compare/cascade-v0.1.107...cascade-v0.1.108) - 2026-06-07

### Other

- *(p2p)* split sync.rs relay/session methods to a submodule; add source-length cap
- extract large test modules to sibling files via #[path]

## [0.1.107](https://github.com/Mearman/cascade/compare/cascade-v0.1.106...cascade-v0.1.107) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.106](https://github.com/Mearman/cascade/compare/cascade-v0.1.105...cascade-v0.1.106) - 2026-06-07

### Other

- *(gdrive)* remove the TLS deadlock workaround, collapse onto one shared pooled client

## [0.1.105](https://github.com/Mearman/cascade/compare/cascade-v0.1.104...cascade-v0.1.105) - 2026-06-07

### Added

- *(gdrive)* make the shared pooled HTTP client the default; workaround is now an escape hatch

## [0.1.104](https://github.com/Mearman/cascade/compare/cascade-v0.1.103...cascade-v0.1.104) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.103](https://github.com/Mearman/cascade/compare/cascade-v0.1.102...cascade-v0.1.103) - 2026-06-07

### Added

- *(cascade)* wire pooled-shared mode through daemon start and backend factory

## [0.1.102](https://github.com/Mearman/cascade/compare/cascade-v0.1.101...cascade-v0.1.102) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.101](https://github.com/Mearman/cascade/compare/cascade-v0.1.100...cascade-v0.1.101) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.100](https://github.com/Mearman/cascade/compare/cascade-v0.1.99...cascade-v0.1.100) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.99](https://github.com/Mearman/cascade/compare/cascade-v0.1.98...cascade-v0.1.99) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.98](https://github.com/Mearman/cascade/compare/cascade-v0.1.97...cascade-v0.1.98) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.97](https://github.com/Mearman/cascade/compare/cascade-v0.1.96...cascade-v0.1.97) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.96](https://github.com/Mearman/cascade/compare/cascade-v0.1.95...cascade-v0.1.96) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.95](https://github.com/Mearman/cascade/compare/cascade-v0.1.94...cascade-v0.1.95) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.94](https://github.com/Mearman/cascade/compare/cascade-v0.1.93...cascade-v0.1.94) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.93](https://github.com/Mearman/cascade/compare/cascade-v0.1.92...cascade-v0.1.93) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.92](https://github.com/Mearman/cascade/compare/cascade-v0.1.91...cascade-v0.1.92) - 2026-06-07

### Other

- update Cargo.toml dependencies

## [0.1.91](https://github.com/Mearman/cascade/compare/cascade-v0.1.90...cascade-v0.1.91) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.90](https://github.com/Mearman/cascade/compare/cascade-v0.1.89...cascade-v0.1.90) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.89](https://github.com/Mearman/cascade/compare/cascade-v0.1.88...cascade-v0.1.89) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.88](https://github.com/Mearman/cascade/compare/cascade-v0.1.87...cascade-v0.1.88) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.87](https://github.com/Mearman/cascade/compare/cascade-v0.1.86...cascade-v0.1.87) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.86](https://github.com/Mearman/cascade/compare/cascade-v0.1.85...cascade-v0.1.86) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.85](https://github.com/Mearman/cascade/compare/cascade-v0.1.84...cascade-v0.1.85) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.84](https://github.com/Mearman/cascade/compare/cascade-v0.1.83...cascade-v0.1.84) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.83](https://github.com/Mearman/cascade/compare/cascade-v0.1.82...cascade-v0.1.83) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.82](https://github.com/Mearman/cascade/compare/cascade-v0.1.81...cascade-v0.1.82) - 2026-06-06

### Other

- update Cargo.toml dependencies

## [0.1.81](https://github.com/Mearman/cascade/compare/cascade-v0.1.80...cascade-v0.1.81) - 2026-06-06

### Fixed

- *(clippy)* suppress trivially_copy_pass_by_ref on cfg-polymorphic no-op stub
- *(cli)* align mark_web_ready signature across cfg variants

### Other

- *(engine)* port engine/cache/sync to portable traits for WASM
- integrate max-file-length branch into main
- *(presenters)* update all presenters to portable Backend trait signatures

## [0.1.80](https://github.com/Mearman/cascade/compare/cascade-v0.1.79...cascade-v0.1.80) - 2026-06-05

### Other

- update Cargo.toml dependencies

## [0.1.79](https://github.com/Mearman/cascade/compare/cascade-v0.1.78...cascade-v0.1.79) - 2026-06-05

### Added

- *(auth)* PWA authentication via pairing code, shared secret, and device code

## [0.1.78](https://github.com/Mearman/cascade/compare/cascade-v0.1.77...cascade-v0.1.78) - 2026-06-05

### Other

- update Cargo.toml dependencies

## [0.1.77](https://github.com/Mearman/cascade/compare/cascade-v0.1.76...cascade-v0.1.77) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.76](https://github.com/Mearman/cascade/compare/cascade-v0.1.75...cascade-v0.1.76) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.75](https://github.com/Mearman/cascade/compare/cascade-v0.1.74...cascade-v0.1.75) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.74](https://github.com/Mearman/cascade/compare/cascade-v0.1.73...cascade-v0.1.74) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.73](https://github.com/Mearman/cascade/compare/cascade-v0.1.72...cascade-v0.1.73) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.72](https://github.com/Mearman/cascade/compare/cascade-v0.1.71...cascade-v0.1.72) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.71](https://github.com/Mearman/cascade/compare/cascade-v0.1.70...cascade-v0.1.71) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.70](https://github.com/Mearman/cascade/compare/cascade-v0.1.69...cascade-v0.1.70) - 2026-06-04

### Added

- *(web-api)* add daemon HTTP JSON API for the v1 PWA

### Fixed

- *(clippy)* pass WebRuntimeOpt by value in mark_web_ready

## [0.1.69](https://github.com/Mearman/cascade/compare/cascade-v0.1.68...cascade-v0.1.69) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.68](https://github.com/Mearman/cascade/compare/cascade-v0.1.67...cascade-v0.1.68) - 2026-06-04

### Other

- update Cargo.toml dependencies

## [0.1.67](https://github.com/Mearman/cascade/compare/cascade-v0.1.66...cascade-v0.1.67) - 2026-06-04

### Fixed

- *(directional-share)* block node-wide data-verb grants and tokens (F4)
- *(directional-share)* bind grant scope to the canonical BEP folder id (F1)

### Other

- apply cargo fmt to directional-share fix files
- *(p2p)* directional data-sharing implementation (FAILS ADVERSARIAL REVIEW — do not merge)

## [0.1.66](https://github.com/Mearman/cascade/compare/cascade-v0.1.65...cascade-v0.1.66) - 2026-06-03

### Added

- *(cascade)* add a headless daemon Docker image for NAS deployment
- *(cascade)* express P2P posture and relay in config and honour --no-mount on Linux

### Fixed

- *(cascade)* satisfy Linux-gated clippy in the start fallback chain

## [0.1.65](https://github.com/Mearman/cascade/compare/cascade-v0.1.64...cascade-v0.1.65) - 2026-06-03

### Other

- update Cargo.toml dependencies

## [0.1.64](https://github.com/Mearman/cascade/compare/cascade-v0.1.63...cascade-v0.1.64) - 2026-06-03

### Other

- update Cargo.toml dependencies

## [0.1.63](https://github.com/Mearman/cascade/compare/cascade-v0.1.62...cascade-v0.1.63) - 2026-06-03

### Fixed

- *(cascade)* bound daemon shutdown so stop can't leave a hung process

### Other

- *(cascade)* wrap the shutdown timeout chain per rustfmt

## [0.1.62](https://github.com/Mearman/cascade/compare/cascade-v0.1.61...cascade-v0.1.62) - 2026-06-03

### Other

- update Cargo.toml dependencies

## [0.1.61](https://github.com/Mearman/cascade/compare/cascade-v0.1.60...cascade-v0.1.61) - 2026-06-03

### Added

- *(cli-service)* implement the Linux system scope for cascade service

### Fixed

- *(cli-service)* correct the desktop scope prompt for Linux system support

### Other

- *(cli-service)* record Linux system scope as implemented

## [0.1.60](https://github.com/Mearman/cascade/compare/cascade-v0.1.59...cascade-v0.1.60) - 2026-06-03

### Added

- *(cascade)* resolve the service install scope from flags, session, and a desktop prompt
- *(cascade)* implement the Windows per-user scheduled-task service backend
- *(cascade)* implement the Linux systemd --user service backend
- *(cascade)* implement the macOS launchd service backend
- *(cascade)* wire cascade service into the clap command tree
- *(cascade)* scaffold the service module framework

### Fixed

- *(cascade)* satisfy Windows-gated clippy in the service backend
- *(cascade)* make async-trait an unconditional dependency
- *(cascade)* exit cleanly when no backends are configured

## [0.1.59](https://github.com/Mearman/cascade/compare/cascade-v0.1.58...cascade-v0.1.59) - 2026-06-03

### Other

- update Cargo.toml dependencies

## [0.1.58](https://github.com/Mearman/cascade/compare/cascade-v0.1.57...cascade-v0.1.58) - 2026-06-03

### Added

- *(cli)* cascade token issue/revoke/list and a --token flag on remote
- *(cascade)* surface the remote management verbs in the manager CLI
- *(cascade)* inject the backend factory for runtime BackendAdd
- *(cascade)* wire the management dispatcher into daemon backends at startup
- *(cascade)* add grant and remote management-plane CLI commands
- *(cascade)* wire engine-backed ContentProvider into ProjFS mount
- *(cascade)* serve the File Provider RPC bridge from the daemon
- *(cli)* prompt for device_name and per-peer name in p2p backend-add
- *(presenter-projfs)* wire ProjFS as the preferred Windows presenter
- *(p2p)* add invalid and no_permissions flags to FileInfo
- *(p2p)* add per-row sequence to FileInfo for delta sync
- *(p2p)* add request_id field to BEP Request/Response
- *(p2p)* add Version vector type and FileInfo field
- *(p2p)* add FileInfo.deleted flag to BEP wire protocol
- *(cli)* support p2p backend in backend-add wizard
- *(p2p)* unit + integration + Docker Compose e2e for the P2P backend
- *(p2p)* add cascade-backend-p2p crate with content-addressed storage
- *(cli)* enable P2P optimisation layer via [p2p] config and --p2p flag
- *(cli,ci)* expose local backend and add end-to-end smoke tests
- *(mount)* implement NFS mount on Linux
- *(mount)* implement cascade stop on Windows via taskkill
- *(mount)* wire FUSE on Linux and net-use WebDAV on Windows
- *(cli)* add --no-mount flag to skip macOS WebDAV/NFS auto-mount
- *(cli)* unmount WebDAV mount on cascade stop
- *(webdav)* lazy-load directory contents on demand
- *(mount)* add FSKit as primary macOS presenter with proper fallback cleanup
- *(mount)* prefer NFSv4 on macOS, fall back to v3 with escalation
- *(mount)* use WebDAV presenter on macOS, NFS fallback elsewhere
- add cascade-presenter-webdav to workspace members
- *(mount)* retry NFS mount with admin privileges on macOS
- *(auth)* support user-provided OAuth clients
- *(auth)* add localhost redirect OAuth2 flow and compile-time credentials
- *(init)* add non-interactive flags for scripted setup
- *(mount)* wire NFS presenter into sync runner
- *(cli)* implement cache warm and cache clear commands
- *(cascade)* implement PID file and SIGTERM for cascade stop
- *(cascade)* prompt for type-specific credentials in backend add
- *(cascade)* prompt for S3 credentials in init wizard
- *(cascade)* wire multi-backend dispatcher into cascade start
- *(cascade)* add backend-auth command for gdrive OAuth device-code flow
- *(cli)* add cascade init command with guided setup
- *(ci)* maximise strictness of all quality gates
- add backend-add and backend-remove CLI commands
- sync runner auto-pins new files, mount starts cache manager
- wire pin/unpin/pin-list/cache CLI commands to cache manager
- add cache manager with pinning, eviction, and lifecycle policies
- *(cli)* implement status and backend-list with real state DB queries
- *(nfs)* bridge NFS procedures to VFS tree via NfsContext
- *(engine)* integrate .cascade config filtering into sync runner
- *(cli)* wire start command to NFS server, sync runner, and GDrive backend
- *(cli)* cascade binary with clap commands and integration tests

### Fixed

- *(cascade)* shut the daemon down gracefully on SIGTERM as well as SIGINT
- *(cascade)* fully-qualify the DeviceIdentity intra-doc link in grant.rs
- *(engine-manage)* sign capability tokens with the node's real device key
- *(engine)* confine pushed config rules and stop the sync runner
- *(cascade)* use a TOML literal string for the Windows path in grant test seed
- *(cascade)* fully-qualify ProjFS provider module-doc links
- *(cascade)* correct ProjFS runtime note and assert the multi-thread flavour
- *(cascade)* panic-proof the ProjFS provider runtime bridge and validate downloads
- *(cascade)* backtick FSKit in non-macOS test doc
- *(p2p)* surface review-flagged recommended issues from roadmap batch
- *(cli)* tighten p2p backend-add wizard output and discovery handling
- *(ci)* silence Linux unmount_path clippy lint and gate Windows-unused import
- *(mount)* doc link only resolves on macOS; describe behaviour inline instead
- *(mount)* silence Windows clippy on unused try_nfs/mount_nfs and doc backticks
- *(mount)* include Windows in VfsPresenter import for try_webdav
- *(mount)* bring VfsPresenter into scope on Linux and WebDavPresenter on Windows
- *(cli)* allow non-const fn on is_process_alive (unix branch isn't const)
- *(mount)* silence dead_code on Windows for unix-only stop helper
- *(mount)* recover from stale WebDAV mounts whose server is dead
- *(mount)* force-evict dead WebDAV mounts and handle residual EEXIST
- *(auth)* replace silent OAuth fallback with explicit --device-code flag
- *(mount)* evict stale mounts on startup and use localhost for WebDAV
- *(webdav)* avoid blocking the tokio runtime while wiring backends
- *(ci)* replace platform-specific doc link with generic description
- *(ci)* allow missing_const_for_fn on non-macOS unmount_path stub
- *(ci)* gate VfsPresenter and WebDavPresenter imports to macOS only
- *(ci)* gate macOS-only tests behind cfg target_os, fix clippy in presenter-webdav
- *(mount)* resolve nfsv4/webdav conflict — WebDAV → NFS fallback chain on macOS
- *(ci)* suppress missing_const_for_fn and unnecessary_wraps on non-macOS stubs
- *(cli)* suppress dead_code for is_mounted stub on non-macOS
- *(cli)* remove unimplemented backend type claims from help text and display
- *(mount)* gate NFS helpers behind #[cfg(target_os = "macos")]
- *(mount)* guard against double-start using PID file liveness check
- *(init)* register backend in state DB after writing config
- *(backend)* sync config.toml on backend-add and backend-remove
- *(cache)* use config_dir() helper for DB path in open_db()
- *(init)* write gdrive credential file during cascade init
- *(cli)* check PID file liveness in cascade status
- *(cascade)* gate nix dependency and stop() on unix cfg
- *(cascade)* clean up pre-existing clippy errors in test files
- *(cascade)* fail loudly on missing config files and remove unsupported local backend
- *(auth)* platform-guard token storage and redact OAuthConfig debug output
- *(cascade)* resolve clippy errors in CLI modules
- *(cascade)* resolve test compile errors after sync API changes
- resolve reconciliation conflicts and RwLock type mismatch
- *(fuse)* use Generation newtype and commit mount.rs format
- resolve reconciliation conflicts in mount CLI, clippy warnings

### Other

- release v0.1.57
- release v0.1.56
- release v0.1.55
- release v0.1.54
- release v0.1.53
- release v0.1.52
- *(presenter-nfs)* type the NFS cache mode as an enum
- *(cascade)* use human-readable cache values in TOML fixture
- *(cascade-config)* type cache posture and quantity settings
- *(backend-p2p)* collapse connectivity flags into a DiscoveryReach posture
- release v0.1.51
- release v0.1.50
- release v0.1.49
- release v0.1.48
- release v0.1.47
- *(projfs-provider)* serve reads via Backend::read_range
- release v0.1.46
- release v0.1.45
- release v0.1.44
- release v0.1.43
- release v0.1.42
- release v0.1.41
- enforce clippy on all targets, scope restriction lints out of tests
- release v0.1.40
- release v0.1.39
- release v0.1.38
- release v0.1.37
- release v0.1.36
- release v0.1.35
- release v0.1.34
- release v0.1.33
- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- release v0.1.27
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- *(presenter-projfs)* apply rustfmt to try_projfs Arc construction
- *(p2p)* cover WAN gossip end-to-end and in encode/decode round-trips
- release v0.1.21
- *(cli)* cover p2p backend-add config round-trip
- release v0.1.20
- release v0.1.19
- release v0.1.18
- release v0.1.17
- release v0.1.16
- release v0.1.15
- release v0.1.14
- release v0.1.13
- release v0.1.12
- release v0.1.11
- release v0.1.10
- release v0.1.9
- *(build)* hoist dirs to workspace deps and fix lint comment
- *(mount)* cover Windows stop and Linux NFS command construction
- *(cli)* split is_process_alive by cfg to drop #[allow]
- release v0.1.8
- *(expr)* gate DISK.free assertion to unix where statfs is implemented
- cargo fmt
- *(mount)* silence dead_code on Windows for make_ctx (only used by gated tests)
- *(mount)* gate stop tests to unix where stop is actually implemented
- release v0.1.7
- cargo fmt
- *(mount)* replace raw fs calls with path-aware error helpers
- release v0.1.6
- release v0.1.5
- release v0.1.4
- release v0.1.3
- release v0.1.2
- release v0.1.1
- remove hardcoded toolchain paths from git hooks
- *(engine)* separate sync runner from Engine::start()
- *(cli)* add integration tests for CLI command functions
- fmt after CliContext refactor
- *(cli)* introduce CliContext for shared config paths and verbosity
- apply cargo fmt across workspace
- apply automated clippy fixes across workspace
- fmt fixes from shared compilation CI
- add engine lifecycle and init config integration tests
- *(cli)* use Engine struct in mount command
- add NFS/FUSE presenter integration tests
- add P2P integration, e2e, and property tests
- add 11 integration tests for expression evaluation and providers
- add 8 integration tests for conflict detection and multi-backend VFS
- add 8 integration tests for cache lifecycle
- add integration tests for config, state DB, and VFS routing
- clippy and rustfmt pass across workspace
- add .cascade fixture files for config parser integration tests

## [0.1.57](https://github.com/Mearman/cascade/compare/v0.1.56...v0.1.57) - 2026-06-03

### Added

- *(cli)* cascade token issue/revoke/list and a --token flag on remote
- *(cascade)* surface the remote management verbs in the manager CLI
- *(cascade)* inject the backend factory for runtime BackendAdd
- *(cascade)* wire the management dispatcher into daemon backends at startup
- *(cascade)* add grant and remote management-plane CLI commands
- *(cascade)* wire engine-backed ContentProvider into ProjFS mount
- *(cascade)* serve the File Provider RPC bridge from the daemon
- *(cli)* prompt for device_name and per-peer name in p2p backend-add
- *(presenter-projfs)* wire ProjFS as the preferred Windows presenter
- *(p2p)* add invalid and no_permissions flags to FileInfo
- *(p2p)* add per-row sequence to FileInfo for delta sync
- *(p2p)* add request_id field to BEP Request/Response
- *(p2p)* add Version vector type and FileInfo field
- *(p2p)* add FileInfo.deleted flag to BEP wire protocol
- *(cli)* support p2p backend in backend-add wizard
- *(p2p)* unit + integration + Docker Compose e2e for the P2P backend
- *(p2p)* add cascade-backend-p2p crate with content-addressed storage
- *(cli)* enable P2P optimisation layer via [p2p] config and --p2p flag
- *(cli,ci)* expose local backend and add end-to-end smoke tests
- *(mount)* implement NFS mount on Linux
- *(mount)* implement cascade stop on Windows via taskkill
- *(mount)* wire FUSE on Linux and net-use WebDAV on Windows
- *(cli)* add --no-mount flag to skip macOS WebDAV/NFS auto-mount
- *(cli)* unmount WebDAV mount on cascade stop
- *(webdav)* lazy-load directory contents on demand
- *(mount)* add FSKit as primary macOS presenter with proper fallback cleanup
- *(mount)* prefer NFSv4 on macOS, fall back to v3 with escalation
- *(mount)* use WebDAV presenter on macOS, NFS fallback elsewhere
- add cascade-presenter-webdav to workspace members
- *(mount)* retry NFS mount with admin privileges on macOS
- *(auth)* support user-provided OAuth clients
- *(auth)* add localhost redirect OAuth2 flow and compile-time credentials
- *(init)* add non-interactive flags for scripted setup
- *(mount)* wire NFS presenter into sync runner
- *(cli)* implement cache warm and cache clear commands
- *(cascade)* implement PID file and SIGTERM for cascade stop
- *(cascade)* prompt for type-specific credentials in backend add
- *(cascade)* prompt for S3 credentials in init wizard
- *(cascade)* wire multi-backend dispatcher into cascade start
- *(cascade)* add backend-auth command for gdrive OAuth device-code flow
- *(cli)* add cascade init command with guided setup
- *(ci)* maximise strictness of all quality gates
- add backend-add and backend-remove CLI commands
- sync runner auto-pins new files, mount starts cache manager
- wire pin/unpin/pin-list/cache CLI commands to cache manager
- add cache manager with pinning, eviction, and lifecycle policies
- *(cli)* implement status and backend-list with real state DB queries
- *(nfs)* bridge NFS procedures to VFS tree via NfsContext
- *(engine)* integrate .cascade config filtering into sync runner
- *(cli)* wire start command to NFS server, sync runner, and GDrive backend
- *(cli)* cascade binary with clap commands and integration tests

### Fixed

- *(cascade)* shut the daemon down gracefully on SIGTERM as well as SIGINT
- *(cascade)* fully-qualify the DeviceIdentity intra-doc link in grant.rs
- *(engine-manage)* sign capability tokens with the node's real device key
- *(engine)* confine pushed config rules and stop the sync runner
- *(cascade)* use a TOML literal string for the Windows path in grant test seed
- *(cascade)* fully-qualify ProjFS provider module-doc links
- *(cascade)* correct ProjFS runtime note and assert the multi-thread flavour
- *(cascade)* panic-proof the ProjFS provider runtime bridge and validate downloads
- *(cascade)* backtick FSKit in non-macOS test doc
- *(p2p)* surface review-flagged recommended issues from roadmap batch
- *(cli)* tighten p2p backend-add wizard output and discovery handling
- *(ci)* silence Linux unmount_path clippy lint and gate Windows-unused import
- *(mount)* doc link only resolves on macOS; describe behaviour inline instead
- *(mount)* silence Windows clippy on unused try_nfs/mount_nfs and doc backticks
- *(mount)* include Windows in VfsPresenter import for try_webdav
- *(mount)* bring VfsPresenter into scope on Linux and WebDavPresenter on Windows
- *(cli)* allow non-const fn on is_process_alive (unix branch isn't const)
- *(mount)* silence dead_code on Windows for unix-only stop helper
- *(mount)* recover from stale WebDAV mounts whose server is dead
- *(mount)* force-evict dead WebDAV mounts and handle residual EEXIST
- *(auth)* replace silent OAuth fallback with explicit --device-code flag
- *(mount)* evict stale mounts on startup and use localhost for WebDAV
- *(webdav)* avoid blocking the tokio runtime while wiring backends
- *(ci)* replace platform-specific doc link with generic description
- *(ci)* allow missing_const_for_fn on non-macOS unmount_path stub
- *(ci)* gate VfsPresenter and WebDavPresenter imports to macOS only
- *(ci)* gate macOS-only tests behind cfg target_os, fix clippy in presenter-webdav
- *(mount)* resolve nfsv4/webdav conflict — WebDAV → NFS fallback chain on macOS
- *(ci)* suppress missing_const_for_fn and unnecessary_wraps on non-macOS stubs
- *(cli)* suppress dead_code for is_mounted stub on non-macOS
- *(cli)* remove unimplemented backend type claims from help text and display
- *(mount)* gate NFS helpers behind #[cfg(target_os = "macos")]
- *(mount)* guard against double-start using PID file liveness check
- *(init)* register backend in state DB after writing config
- *(backend)* sync config.toml on backend-add and backend-remove
- *(cache)* use config_dir() helper for DB path in open_db()
- *(init)* write gdrive credential file during cascade init
- *(cli)* check PID file liveness in cascade status
- *(cascade)* gate nix dependency and stop() on unix cfg
- *(cascade)* clean up pre-existing clippy errors in test files
- *(cascade)* fail loudly on missing config files and remove unsupported local backend
- *(auth)* platform-guard token storage and redact OAuthConfig debug output
- *(cascade)* resolve clippy errors in CLI modules
- *(cascade)* resolve test compile errors after sync API changes
- resolve reconciliation conflicts and RwLock type mismatch
- *(fuse)* use Generation newtype and commit mount.rs format
- resolve reconciliation conflicts in mount CLI, clippy warnings

### Other

- release v0.1.56
- release v0.1.55
- release v0.1.54
- release v0.1.53
- release v0.1.52
- *(presenter-nfs)* type the NFS cache mode as an enum
- *(cascade)* use human-readable cache values in TOML fixture
- *(cascade-config)* type cache posture and quantity settings
- *(backend-p2p)* collapse connectivity flags into a DiscoveryReach posture
- release v0.1.51
- release v0.1.50
- release v0.1.49
- release v0.1.48
- release v0.1.47
- *(projfs-provider)* serve reads via Backend::read_range
- release v0.1.46
- release v0.1.45
- release v0.1.44
- release v0.1.43
- release v0.1.42
- release v0.1.41
- enforce clippy on all targets, scope restriction lints out of tests
- release v0.1.40
- release v0.1.39
- release v0.1.38
- release v0.1.37
- release v0.1.36
- release v0.1.35
- release v0.1.34
- release v0.1.33
- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- release v0.1.27
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- *(presenter-projfs)* apply rustfmt to try_projfs Arc construction
- *(p2p)* cover WAN gossip end-to-end and in encode/decode round-trips
- release v0.1.21
- *(cli)* cover p2p backend-add config round-trip
- release v0.1.20
- release v0.1.19
- release v0.1.18
- release v0.1.17
- release v0.1.16
- release v0.1.15
- release v0.1.14
- release v0.1.13
- release v0.1.12
- release v0.1.11
- release v0.1.10
- release v0.1.9
- *(build)* hoist dirs to workspace deps and fix lint comment
- *(mount)* cover Windows stop and Linux NFS command construction
- *(cli)* split is_process_alive by cfg to drop #[allow]
- release v0.1.8
- *(expr)* gate DISK.free assertion to unix where statfs is implemented
- cargo fmt
- *(mount)* silence dead_code on Windows for make_ctx (only used by gated tests)
- *(mount)* gate stop tests to unix where stop is actually implemented
- release v0.1.7
- cargo fmt
- *(mount)* replace raw fs calls with path-aware error helpers
- release v0.1.6
- release v0.1.5
- release v0.1.4
- release v0.1.3
- release v0.1.2
- release v0.1.1
- remove hardcoded toolchain paths from git hooks
- *(engine)* separate sync runner from Engine::start()
- *(cli)* add integration tests for CLI command functions
- fmt after CliContext refactor
- *(cli)* introduce CliContext for shared config paths and verbosity
- apply cargo fmt across workspace
- apply automated clippy fixes across workspace
- fmt fixes from shared compilation CI
- add engine lifecycle and init config integration tests
- *(cli)* use Engine struct in mount command
- add NFS/FUSE presenter integration tests
- add P2P integration, e2e, and property tests
- add 11 integration tests for expression evaluation and providers
- add 8 integration tests for conflict detection and multi-backend VFS
- add 8 integration tests for cache lifecycle
- add integration tests for config, state DB, and VFS routing
- clippy and rustfmt pass across workspace
- add .cascade fixture files for config parser integration tests

## [0.1.56](https://github.com/Mearman/cascade/compare/cascade-v0.1.55...cascade-v0.1.56) - 2026-06-03

### Other

- update Cargo.toml dependencies

## [0.1.55](https://github.com/Mearman/cascade/compare/cascade-v0.1.54...cascade-v0.1.55) - 2026-06-03

### Fixed

- *(cascade)* shut the daemon down gracefully on SIGTERM as well as SIGINT

## [0.1.54](https://github.com/Mearman/cascade/compare/cascade-v0.1.53...cascade-v0.1.54) - 2026-06-03

### Added

- *(cli)* cascade token issue/revoke/list and a --token flag on remote

### Fixed

- *(cascade)* fully-qualify the DeviceIdentity intra-doc link in grant.rs
- *(engine-manage)* sign capability tokens with the node's real device key

## [0.1.53](https://github.com/Mearman/cascade/compare/cascade-v0.1.52...cascade-v0.1.53) - 2026-06-02

### Other

- update Cargo.toml dependencies

## [0.1.52](https://github.com/Mearman/cascade/compare/cascade-v0.1.51...cascade-v0.1.52) - 2026-06-02

### Other

- *(presenter-nfs)* type the NFS cache mode as an enum
- *(cascade)* use human-readable cache values in TOML fixture
- *(cascade-config)* type cache posture and quantity settings
- *(backend-p2p)* collapse connectivity flags into a DiscoveryReach posture

## [0.1.51](https://github.com/Mearman/cascade/compare/cascade-v0.1.50...cascade-v0.1.51) - 2026-06-02

### Added

- *(cascade)* surface the remote management verbs in the manager CLI
- *(cascade)* inject the backend factory for runtime BackendAdd

### Fixed

- *(engine)* confine pushed config rules and stop the sync runner

## [0.1.50](https://github.com/Mearman/cascade/compare/cascade-v0.1.49...cascade-v0.1.50) - 2026-06-02

### Other

- update Cargo.toml dependencies

## [0.1.49](https://github.com/Mearman/cascade/compare/cascade-v0.1.48...cascade-v0.1.49) - 2026-06-02

### Added

- *(cascade)* wire the management dispatcher into daemon backends at startup
- *(cascade)* add grant and remote management-plane CLI commands

### Fixed

- *(cascade)* use a TOML literal string for the Windows path in grant test seed

## [0.1.48](https://github.com/Mearman/cascade/compare/cascade-v0.1.47...cascade-v0.1.48) - 2026-06-02

### Other

- update Cargo.toml dependencies

## [0.1.47](https://github.com/Mearman/cascade/compare/cascade-v0.1.46...cascade-v0.1.47) - 2026-06-02

### Other

- *(projfs-provider)* serve reads via Backend::read_range

## [0.1.46](https://github.com/Mearman/cascade/compare/cascade-v0.1.45...cascade-v0.1.46) - 2026-06-02

### Other

- update Cargo.toml dependencies

## [0.1.45](https://github.com/Mearman/cascade/compare/cascade-v0.1.44...cascade-v0.1.45) - 2026-06-02

### Other

- update Cargo.toml dependencies

## [0.1.44](https://github.com/Mearman/cascade/compare/cascade-v0.1.43...cascade-v0.1.44) - 2026-06-02

### Other

- update Cargo.toml dependencies

## [0.1.43](https://github.com/Mearman/cascade/compare/cascade-v0.1.42...cascade-v0.1.43) - 2026-06-02

### Added

- *(cascade)* wire engine-backed ContentProvider into ProjFS mount

### Fixed

- *(cascade)* fully-qualify ProjFS provider module-doc links
- *(cascade)* correct ProjFS runtime note and assert the multi-thread flavour
- *(cascade)* panic-proof the ProjFS provider runtime bridge and validate downloads

## [0.1.42](https://github.com/Mearman/cascade/compare/cascade-v0.1.41...cascade-v0.1.42) - 2026-06-02

### Other

- update Cargo.toml dependencies

## [0.1.41](https://github.com/Mearman/cascade/compare/cascade-v0.1.40...cascade-v0.1.41) - 2026-06-02

### Fixed

- *(cascade)* backtick FSKit in non-macOS test doc

### Other

- enforce clippy on all targets, scope restriction lints out of tests

## [0.1.40](https://github.com/Mearman/cascade/compare/cascade-v0.1.39...cascade-v0.1.40) - 2026-06-01

### Added

- *(cascade)* serve the File Provider RPC bridge from the daemon

## [0.1.39](https://github.com/Mearman/cascade/compare/cascade-v0.1.38...cascade-v0.1.39) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.38](https://github.com/Mearman/cascade/compare/cascade-v0.1.37...cascade-v0.1.38) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.37](https://github.com/Mearman/cascade/compare/cascade-v0.1.36...cascade-v0.1.37) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.36](https://github.com/Mearman/cascade/compare/cascade-v0.1.35...cascade-v0.1.36) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.35](https://github.com/Mearman/cascade/compare/cascade-v0.1.34...cascade-v0.1.35) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.34](https://github.com/Mearman/cascade/compare/cascade-v0.1.33...cascade-v0.1.34) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.33](https://github.com/Mearman/cascade/compare/cascade-v0.1.32...cascade-v0.1.33) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.32](https://github.com/Mearman/cascade/compare/cascade-v0.1.31...cascade-v0.1.32) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.31](https://github.com/Mearman/cascade/compare/cascade-v0.1.30...cascade-v0.1.31) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.30](https://github.com/Mearman/cascade/compare/cascade-v0.1.29...cascade-v0.1.30) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.29](https://github.com/Mearman/cascade/compare/cascade-v0.1.28...cascade-v0.1.29) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.28](https://github.com/Mearman/cascade/compare/cascade-v0.1.27...cascade-v0.1.28) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.27](https://github.com/Mearman/cascade/compare/cascade-v0.1.26...cascade-v0.1.27) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.26](https://github.com/Mearman/cascade/compare/cascade-v0.1.25...cascade-v0.1.26) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.25](https://github.com/Mearman/cascade/compare/cascade-v0.1.24...cascade-v0.1.25) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.24](https://github.com/Mearman/cascade/compare/cascade-v0.1.23...cascade-v0.1.24) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.23](https://github.com/Mearman/cascade/compare/cascade-v0.1.22...cascade-v0.1.23) - 2026-06-01

### Other

- update Cargo.toml dependencies

## [0.1.22](https://github.com/Mearman/cascade/compare/cascade-v0.1.21...cascade-v0.1.22) - 2026-06-01

### Added

- *(cli)* prompt for device_name and per-peer name in p2p backend-add
- *(presenter-projfs)* wire ProjFS as the preferred Windows presenter
- *(p2p)* add invalid and no_permissions flags to FileInfo
- *(p2p)* add per-row sequence to FileInfo for delta sync
- *(p2p)* add request_id field to BEP Request/Response
- *(p2p)* add Version vector type and FileInfo field
- *(p2p)* add FileInfo.deleted flag to BEP wire protocol
- *(cli)* support p2p backend in backend-add wizard
- *(p2p)* unit + integration + Docker Compose e2e for the P2P backend
- *(p2p)* add cascade-backend-p2p crate with content-addressed storage
- *(cli)* enable P2P optimisation layer via [p2p] config and --p2p flag
- *(cli,ci)* expose local backend and add end-to-end smoke tests
- *(mount)* implement NFS mount on Linux
- *(mount)* implement cascade stop on Windows via taskkill
- *(mount)* wire FUSE on Linux and net-use WebDAV on Windows
- *(cli)* add --no-mount flag to skip macOS WebDAV/NFS auto-mount
- *(cli)* unmount WebDAV mount on cascade stop
- *(webdav)* lazy-load directory contents on demand
- *(mount)* add FSKit as primary macOS presenter with proper fallback cleanup
- *(mount)* prefer NFSv4 on macOS, fall back to v3 with escalation
- *(mount)* use WebDAV presenter on macOS, NFS fallback elsewhere
- add cascade-presenter-webdav to workspace members
- *(mount)* retry NFS mount with admin privileges on macOS
- *(auth)* support user-provided OAuth clients
- *(auth)* add localhost redirect OAuth2 flow and compile-time credentials
- *(init)* add non-interactive flags for scripted setup
- *(mount)* wire NFS presenter into sync runner
- *(cli)* implement cache warm and cache clear commands
- *(cascade)* implement PID file and SIGTERM for cascade stop
- *(cascade)* prompt for type-specific credentials in backend add
- *(cascade)* prompt for S3 credentials in init wizard
- *(cascade)* wire multi-backend dispatcher into cascade start
- *(cascade)* add backend-auth command for gdrive OAuth device-code flow
- *(cli)* add cascade init command with guided setup
- *(ci)* maximise strictness of all quality gates
- add backend-add and backend-remove CLI commands
- sync runner auto-pins new files, mount starts cache manager
- wire pin/unpin/pin-list/cache CLI commands to cache manager
- add cache manager with pinning, eviction, and lifecycle policies
- *(cli)* implement status and backend-list with real state DB queries
- *(nfs)* bridge NFS procedures to VFS tree via NfsContext
- *(engine)* integrate .cascade config filtering into sync runner
- *(cli)* wire start command to NFS server, sync runner, and GDrive backend
- *(cli)* cascade binary with clap commands and integration tests

### Fixed

- *(p2p)* surface review-flagged recommended issues from roadmap batch
- *(cli)* tighten p2p backend-add wizard output and discovery handling
- *(ci)* silence Linux unmount_path clippy lint and gate Windows-unused import
- *(mount)* doc link only resolves on macOS; describe behaviour inline instead
- *(mount)* silence Windows clippy on unused try_nfs/mount_nfs and doc backticks
- *(mount)* include Windows in VfsPresenter import for try_webdav
- *(mount)* bring VfsPresenter into scope on Linux and WebDavPresenter on Windows
- *(cli)* allow non-const fn on is_process_alive (unix branch isn't const)
- *(mount)* silence dead_code on Windows for unix-only stop helper
- *(mount)* recover from stale WebDAV mounts whose server is dead
- *(mount)* force-evict dead WebDAV mounts and handle residual EEXIST
- *(auth)* replace silent OAuth fallback with explicit --device-code flag
- *(mount)* evict stale mounts on startup and use localhost for WebDAV
- *(webdav)* avoid blocking the tokio runtime while wiring backends
- *(ci)* replace platform-specific doc link with generic description
- *(ci)* allow missing_const_for_fn on non-macOS unmount_path stub
- *(ci)* gate VfsPresenter and WebDavPresenter imports to macOS only
- *(ci)* gate macOS-only tests behind cfg target_os, fix clippy in presenter-webdav
- *(mount)* resolve nfsv4/webdav conflict — WebDAV → NFS fallback chain on macOS
- *(ci)* suppress missing_const_for_fn and unnecessary_wraps on non-macOS stubs
- *(cli)* suppress dead_code for is_mounted stub on non-macOS
- *(cli)* remove unimplemented backend type claims from help text and display
- *(mount)* gate NFS helpers behind #[cfg(target_os = "macos")]
- *(mount)* guard against double-start using PID file liveness check
- *(init)* register backend in state DB after writing config
- *(backend)* sync config.toml on backend-add and backend-remove
- *(cache)* use config_dir() helper for DB path in open_db()
- *(init)* write gdrive credential file during cascade init
- *(cli)* check PID file liveness in cascade status
- *(cascade)* gate nix dependency and stop() on unix cfg
- *(cascade)* clean up pre-existing clippy errors in test files
- *(cascade)* fail loudly on missing config files and remove unsupported local backend
- *(auth)* platform-guard token storage and redact OAuthConfig debug output
- *(cascade)* resolve clippy errors in CLI modules
- *(cascade)* resolve test compile errors after sync API changes
- resolve reconciliation conflicts and RwLock type mismatch
- *(fuse)* use Generation newtype and commit mount.rs format
- resolve reconciliation conflicts in mount CLI, clippy warnings

### Other

- *(presenter-projfs)* apply rustfmt to try_projfs Arc construction
- *(p2p)* cover WAN gossip end-to-end and in encode/decode round-trips
- release v0.1.21
- *(cli)* cover p2p backend-add config round-trip
- release v0.1.20
- release v0.1.19
- release v0.1.18
- release v0.1.17
- release v0.1.16
- release v0.1.15
- release v0.1.14
- release v0.1.13
- release v0.1.12
- release v0.1.11
- release v0.1.10
- release v0.1.9
- *(build)* hoist dirs to workspace deps and fix lint comment
- *(mount)* cover Windows stop and Linux NFS command construction
- *(cli)* split is_process_alive by cfg to drop #[allow]
- release v0.1.8
- *(expr)* gate DISK.free assertion to unix where statfs is implemented
- cargo fmt
- *(mount)* silence dead_code on Windows for make_ctx (only used by gated tests)
- *(mount)* gate stop tests to unix where stop is actually implemented
- release v0.1.7
- cargo fmt
- *(mount)* replace raw fs calls with path-aware error helpers
- release v0.1.6
- release v0.1.5
- release v0.1.4
- release v0.1.3
- release v0.1.2
- release v0.1.1
- remove hardcoded toolchain paths from git hooks
- *(engine)* separate sync runner from Engine::start()
- *(cli)* add integration tests for CLI command functions
- fmt after CliContext refactor
- *(cli)* introduce CliContext for shared config paths and verbosity
- apply cargo fmt across workspace
- apply automated clippy fixes across workspace
- fmt fixes from shared compilation CI
- add engine lifecycle and init config integration tests
- *(cli)* use Engine struct in mount command
- add NFS/FUSE presenter integration tests
- add P2P integration, e2e, and property tests
- add 11 integration tests for expression evaluation and providers
- add 8 integration tests for conflict detection and multi-backend VFS
- add 8 integration tests for cache lifecycle
- add integration tests for config, state DB, and VFS routing
- clippy and rustfmt pass across workspace
- add .cascade fixture files for config parser integration tests

## [0.1.21](https://github.com/Mearman/cascade/compare/v0.1.20...v0.1.21) - 2026-05-31

### Added

- *(p2p)* add request_id field to BEP Request/Response
- *(p2p)* add Version vector type and FileInfo field
- *(p2p)* add FileInfo.deleted flag to BEP wire protocol
- *(cli)* support p2p backend in backend-add wizard

### Fixed

- *(cli)* tighten p2p backend-add wizard output and discovery handling

### Other

- *(cli)* cover p2p backend-add config round-trip

## [0.1.20](https://github.com/Mearman/cascade/compare/v0.1.19...v0.1.20) - 2026-05-31

### Added

- *(p2p)* unit + integration + Docker Compose e2e for the P2P backend

## [0.1.19](https://github.com/Mearman/cascade/compare/v0.1.18...v0.1.19) - 2026-05-31

### Other

- update Cargo.toml dependencies

## [0.1.18](https://github.com/Mearman/cascade/compare/v0.1.17...v0.1.18) - 2026-05-31

### Added

- *(p2p)* add cascade-backend-p2p crate with content-addressed storage
- *(cli)* enable P2P optimisation layer via [p2p] config and --p2p flag
- *(cli,ci)* expose local backend and add end-to-end smoke tests
- *(mount)* implement NFS mount on Linux
- *(mount)* implement cascade stop on Windows via taskkill
- *(mount)* wire FUSE on Linux and net-use WebDAV on Windows
- *(cli)* add --no-mount flag to skip macOS WebDAV/NFS auto-mount
- *(cli)* unmount WebDAV mount on cascade stop
- *(webdav)* lazy-load directory contents on demand
- *(mount)* add FSKit as primary macOS presenter with proper fallback cleanup
- *(mount)* prefer NFSv4 on macOS, fall back to v3 with escalation
- *(mount)* use WebDAV presenter on macOS, NFS fallback elsewhere
- add cascade-presenter-webdav to workspace members
- *(mount)* retry NFS mount with admin privileges on macOS
- *(auth)* support user-provided OAuth clients
- *(auth)* add localhost redirect OAuth2 flow and compile-time credentials
- *(init)* add non-interactive flags for scripted setup
- *(mount)* wire NFS presenter into sync runner
- *(cli)* implement cache warm and cache clear commands
- *(cascade)* implement PID file and SIGTERM for cascade stop
- *(cascade)* prompt for type-specific credentials in backend add
- *(cascade)* prompt for S3 credentials in init wizard
- *(cascade)* wire multi-backend dispatcher into cascade start
- *(cascade)* add backend-auth command for gdrive OAuth device-code flow
- *(cli)* add cascade init command with guided setup
- *(ci)* maximise strictness of all quality gates
- add backend-add and backend-remove CLI commands
- sync runner auto-pins new files, mount starts cache manager
- wire pin/unpin/pin-list/cache CLI commands to cache manager
- add cache manager with pinning, eviction, and lifecycle policies
- *(cli)* implement status and backend-list with real state DB queries
- *(nfs)* bridge NFS procedures to VFS tree via NfsContext
- *(engine)* integrate .cascade config filtering into sync runner
- *(cli)* wire start command to NFS server, sync runner, and GDrive backend
- *(cli)* cascade binary with clap commands and integration tests

### Fixed

- *(ci)* silence Linux unmount_path clippy lint and gate Windows-unused import
- *(mount)* doc link only resolves on macOS; describe behaviour inline instead
- *(mount)* silence Windows clippy on unused try_nfs/mount_nfs and doc backticks
- *(mount)* include Windows in VfsPresenter import for try_webdav
- *(mount)* bring VfsPresenter into scope on Linux and WebDavPresenter on Windows
- *(cli)* allow non-const fn on is_process_alive (unix branch isn't const)
- *(mount)* silence dead_code on Windows for unix-only stop helper
- *(mount)* recover from stale WebDAV mounts whose server is dead
- *(mount)* force-evict dead WebDAV mounts and handle residual EEXIST
- *(auth)* replace silent OAuth fallback with explicit --device-code flag
- *(mount)* evict stale mounts on startup and use localhost for WebDAV
- *(webdav)* avoid blocking the tokio runtime while wiring backends
- *(ci)* replace platform-specific doc link with generic description
- *(ci)* allow missing_const_for_fn on non-macOS unmount_path stub
- *(ci)* gate VfsPresenter and WebDavPresenter imports to macOS only
- *(ci)* gate macOS-only tests behind cfg target_os, fix clippy in presenter-webdav
- *(mount)* resolve nfsv4/webdav conflict — WebDAV → NFS fallback chain on macOS
- *(ci)* suppress missing_const_for_fn and unnecessary_wraps on non-macOS stubs
- *(cli)* suppress dead_code for is_mounted stub on non-macOS
- *(cli)* remove unimplemented backend type claims from help text and display
- *(mount)* gate NFS helpers behind #[cfg(target_os = "macos")]
- *(mount)* guard against double-start using PID file liveness check
- *(init)* register backend in state DB after writing config
- *(backend)* sync config.toml on backend-add and backend-remove
- *(cache)* use config_dir() helper for DB path in open_db()
- *(init)* write gdrive credential file during cascade init
- *(cli)* check PID file liveness in cascade status
- *(cascade)* gate nix dependency and stop() on unix cfg
- *(cascade)* clean up pre-existing clippy errors in test files
- *(cascade)* fail loudly on missing config files and remove unsupported local backend
- *(auth)* platform-guard token storage and redact OAuthConfig debug output
- *(cascade)* resolve clippy errors in CLI modules
- *(cascade)* resolve test compile errors after sync API changes
- resolve reconciliation conflicts and RwLock type mismatch
- *(fuse)* use Generation newtype and commit mount.rs format
- resolve reconciliation conflicts in mount CLI, clippy warnings

### Other

- release v0.1.17
- release v0.1.16
- release v0.1.15
- release v0.1.14
- release v0.1.13
- release v0.1.12
- release v0.1.11
- release v0.1.10
- release v0.1.9
- *(build)* hoist dirs to workspace deps and fix lint comment
- *(mount)* cover Windows stop and Linux NFS command construction
- *(cli)* split is_process_alive by cfg to drop #[allow]
- release v0.1.8
- *(expr)* gate DISK.free assertion to unix where statfs is implemented
- cargo fmt
- *(mount)* silence dead_code on Windows for make_ctx (only used by gated tests)
- *(mount)* gate stop tests to unix where stop is actually implemented
- release v0.1.7
- cargo fmt
- *(mount)* replace raw fs calls with path-aware error helpers
- release v0.1.6
- release v0.1.5
- release v0.1.4
- release v0.1.3
- release v0.1.2
- release v0.1.1
- remove hardcoded toolchain paths from git hooks
- *(engine)* separate sync runner from Engine::start()
- *(cli)* add integration tests for CLI command functions
- fmt after CliContext refactor
- *(cli)* introduce CliContext for shared config paths and verbosity
- apply cargo fmt across workspace
- apply automated clippy fixes across workspace
- fmt fixes from shared compilation CI
- add engine lifecycle and init config integration tests
- *(cli)* use Engine struct in mount command
- add NFS/FUSE presenter integration tests
- add P2P integration, e2e, and property tests
- add 11 integration tests for expression evaluation and providers
- add 8 integration tests for conflict detection and multi-backend VFS
- add 8 integration tests for cache lifecycle
- add integration tests for config, state DB, and VFS routing
- clippy and rustfmt pass across workspace
- add .cascade fixture files for config parser integration tests

## [0.1.17](https://github.com/Mearman/cascade/compare/cascade-v0.1.16...cascade-v0.1.17) - 2026-05-31

### Added

- *(p2p)* add cascade-backend-p2p crate with content-addressed storage
- *(cli)* enable P2P optimisation layer via [p2p] config and --p2p flag
- *(cli,ci)* expose local backend and add end-to-end smoke tests
- *(mount)* implement NFS mount on Linux
- *(mount)* implement cascade stop on Windows via taskkill
- *(mount)* wire FUSE on Linux and net-use WebDAV on Windows
- *(cli)* add --no-mount flag to skip macOS WebDAV/NFS auto-mount
- *(cli)* unmount WebDAV mount on cascade stop
- *(webdav)* lazy-load directory contents on demand
- *(mount)* add FSKit as primary macOS presenter with proper fallback cleanup
- *(mount)* prefer NFSv4 on macOS, fall back to v3 with escalation
- *(mount)* use WebDAV presenter on macOS, NFS fallback elsewhere
- add cascade-presenter-webdav to workspace members
- *(mount)* retry NFS mount with admin privileges on macOS
- *(auth)* support user-provided OAuth clients
- *(auth)* add localhost redirect OAuth2 flow and compile-time credentials
- *(init)* add non-interactive flags for scripted setup
- *(mount)* wire NFS presenter into sync runner
- *(cli)* implement cache warm and cache clear commands
- *(cascade)* implement PID file and SIGTERM for cascade stop
- *(cascade)* prompt for type-specific credentials in backend add
- *(cascade)* prompt for S3 credentials in init wizard
- *(cascade)* wire multi-backend dispatcher into cascade start
- *(cascade)* add backend-auth command for gdrive OAuth device-code flow
- *(cli)* add cascade init command with guided setup
- *(ci)* maximise strictness of all quality gates
- add backend-add and backend-remove CLI commands
- sync runner auto-pins new files, mount starts cache manager
- wire pin/unpin/pin-list/cache CLI commands to cache manager
- add cache manager with pinning, eviction, and lifecycle policies
- *(cli)* implement status and backend-list with real state DB queries
- *(nfs)* bridge NFS procedures to VFS tree via NfsContext
- *(engine)* integrate .cascade config filtering into sync runner
- *(cli)* wire start command to NFS server, sync runner, and GDrive backend
- *(cli)* cascade binary with clap commands and integration tests

### Fixed

- *(ci)* silence Linux unmount_path clippy lint and gate Windows-unused import
- *(mount)* doc link only resolves on macOS; describe behaviour inline instead
- *(mount)* silence Windows clippy on unused try_nfs/mount_nfs and doc backticks
- *(mount)* include Windows in VfsPresenter import for try_webdav
- *(mount)* bring VfsPresenter into scope on Linux and WebDavPresenter on Windows
- *(cli)* allow non-const fn on is_process_alive (unix branch isn't const)
- *(mount)* silence dead_code on Windows for unix-only stop helper
- *(mount)* recover from stale WebDAV mounts whose server is dead
- *(mount)* force-evict dead WebDAV mounts and handle residual EEXIST
- *(auth)* replace silent OAuth fallback with explicit --device-code flag
- *(mount)* evict stale mounts on startup and use localhost for WebDAV
- *(webdav)* avoid blocking the tokio runtime while wiring backends
- *(ci)* replace platform-specific doc link with generic description
- *(ci)* allow missing_const_for_fn on non-macOS unmount_path stub
- *(ci)* gate VfsPresenter and WebDavPresenter imports to macOS only
- *(ci)* gate macOS-only tests behind cfg target_os, fix clippy in presenter-webdav
- *(mount)* resolve nfsv4/webdav conflict — WebDAV → NFS fallback chain on macOS
- *(ci)* suppress missing_const_for_fn and unnecessary_wraps on non-macOS stubs
- *(cli)* suppress dead_code for is_mounted stub on non-macOS
- *(cli)* remove unimplemented backend type claims from help text and display
- *(mount)* gate NFS helpers behind #[cfg(target_os = "macos")]
- *(mount)* guard against double-start using PID file liveness check
- *(init)* register backend in state DB after writing config
- *(backend)* sync config.toml on backend-add and backend-remove
- *(cache)* use config_dir() helper for DB path in open_db()
- *(init)* write gdrive credential file during cascade init
- *(cli)* check PID file liveness in cascade status
- *(cascade)* gate nix dependency and stop() on unix cfg
- *(cascade)* clean up pre-existing clippy errors in test files
- *(cascade)* fail loudly on missing config files and remove unsupported local backend
- *(auth)* platform-guard token storage and redact OAuthConfig debug output
- *(cascade)* resolve clippy errors in CLI modules
- *(cascade)* resolve test compile errors after sync API changes
- resolve reconciliation conflicts and RwLock type mismatch
- *(fuse)* use Generation newtype and commit mount.rs format
- resolve reconciliation conflicts in mount CLI, clippy warnings

### Other

- release v0.1.16
- release v0.1.15
- release v0.1.14
- release v0.1.13
- release v0.1.12
- release v0.1.11
- release v0.1.10
- release v0.1.9
- *(build)* hoist dirs to workspace deps and fix lint comment
- *(mount)* cover Windows stop and Linux NFS command construction
- *(cli)* split is_process_alive by cfg to drop #[allow]
- release v0.1.8
- *(expr)* gate DISK.free assertion to unix where statfs is implemented
- cargo fmt
- *(mount)* silence dead_code on Windows for make_ctx (only used by gated tests)
- *(mount)* gate stop tests to unix where stop is actually implemented
- release v0.1.7
- cargo fmt
- *(mount)* replace raw fs calls with path-aware error helpers
- release v0.1.6
- release v0.1.5
- release v0.1.4
- release v0.1.3
- release v0.1.2
- release v0.1.1
- remove hardcoded toolchain paths from git hooks
- *(engine)* separate sync runner from Engine::start()
- *(cli)* add integration tests for CLI command functions
- fmt after CliContext refactor
- *(cli)* introduce CliContext for shared config paths and verbosity
- apply cargo fmt across workspace
- apply automated clippy fixes across workspace
- fmt fixes from shared compilation CI
- add engine lifecycle and init config integration tests
- *(cli)* use Engine struct in mount command
- add NFS/FUSE presenter integration tests
- add P2P integration, e2e, and property tests
- add 11 integration tests for expression evaluation and providers
- add 8 integration tests for conflict detection and multi-backend VFS
- add 8 integration tests for cache lifecycle
- add integration tests for config, state DB, and VFS routing
- clippy and rustfmt pass across workspace
- add .cascade fixture files for config parser integration tests

## [0.1.16](https://github.com/Mearman/cascade/compare/v0.1.15...v0.1.16) - 2026-05-31

### Added

- *(cli)* enable P2P optimisation layer via [p2p] config and --p2p flag

## [0.1.15](https://github.com/Mearman/cascade/compare/v0.1.14...v0.1.15) - 2026-05-31

### Other

- update Cargo.toml dependencies

## [0.1.14](https://github.com/Mearman/cascade/compare/v0.1.13...v0.1.14) - 2026-05-31

### Other

- update Cargo.toml dependencies

## [0.1.13](https://github.com/Mearman/cascade/compare/v0.1.12...v0.1.13) - 2026-05-31

### Other

- update Cargo.toml dependencies

## [0.1.12](https://github.com/Mearman/cascade/compare/v0.1.11...v0.1.12) - 2026-05-31

### Other

- update Cargo.toml dependencies

## [0.1.11](https://github.com/Mearman/cascade/compare/v0.1.10...v0.1.11) - 2026-05-31

### Added

- *(cli,ci)* expose local backend and add end-to-end smoke tests

## [0.1.10](https://github.com/Mearman/cascade/compare/v0.1.9...v0.1.10) - 2026-05-31

### Other

- update Cargo.toml dependencies

## [0.1.9](https://github.com/Mearman/cascade/compare/v0.1.8...v0.1.9) - 2026-05-31

### Added

- *(mount)* implement NFS mount on Linux
- *(mount)* implement cascade stop on Windows via taskkill

### Fixed

- *(ci)* silence Linux unmount_path clippy lint and gate Windows-unused import

### Other

- *(build)* hoist dirs to workspace deps and fix lint comment
- *(mount)* cover Windows stop and Linux NFS command construction
- *(cli)* split is_process_alive by cfg to drop #[allow]

## [0.1.8](https://github.com/Mearman/cascade/compare/v0.1.7...v0.1.8) - 2026-05-31

### Added

- *(mount)* wire FUSE on Linux and net-use WebDAV on Windows

### Fixed

- *(mount)* doc link only resolves on macOS; describe behaviour inline instead
- *(mount)* silence Windows clippy on unused try_nfs/mount_nfs and doc backticks
- *(mount)* include Windows in VfsPresenter import for try_webdav
- *(mount)* bring VfsPresenter into scope on Linux and WebDavPresenter on Windows
- *(cli)* allow non-const fn on is_process_alive (unix branch isn't const)
- *(mount)* silence dead_code on Windows for unix-only stop helper

### Other

- *(expr)* gate DISK.free assertion to unix where statfs is implemented
- cargo fmt
- *(mount)* silence dead_code on Windows for make_ctx (only used by gated tests)
- *(mount)* gate stop tests to unix where stop is actually implemented

## [0.1.7](https://github.com/Mearman/cascade/compare/v0.1.6...v0.1.7) - 2026-05-30

### Added

- *(cli)* add --no-mount flag to skip macOS WebDAV/NFS auto-mount
- *(cli)* unmount WebDAV mount on cascade stop
- *(webdav)* lazy-load directory contents on demand

### Fixed

- *(mount)* recover from stale WebDAV mounts whose server is dead
- *(mount)* force-evict dead WebDAV mounts and handle residual EEXIST
- *(auth)* replace silent OAuth fallback with explicit --device-code flag
- *(mount)* evict stale mounts on startup and use localhost for WebDAV
- *(webdav)* avoid blocking the tokio runtime while wiring backends

### Other

- cargo fmt
- *(mount)* replace raw fs calls with path-aware error helpers

## [0.1.6](https://github.com/Mearman/cascade/compare/v0.1.5...v0.1.6) - 2026-05-29

### Other

- update Cargo.toml dependencies

## [0.1.5](https://github.com/Mearman/cascade/compare/v0.1.4...v0.1.5) - 2026-05-29

### Other

- update Cargo.toml dependencies

## [0.1.4](https://github.com/Mearman/cascade/compare/v0.1.3...v0.1.4) - 2026-05-29

### Other

- update Cargo.toml dependencies

## [0.1.3](https://github.com/Mearman/cascade/compare/v0.1.2...v0.1.3) - 2026-05-29

### Other

- update Cargo.toml dependencies

## [0.1.2](https://github.com/Mearman/cascade/compare/v0.1.1...v0.1.2) - 2026-05-29

### Added

- *(mount)* add FSKit as primary macOS presenter with proper fallback cleanup
- *(mount)* prefer NFSv4 on macOS, fall back to v3 with escalation
- *(mount)* use WebDAV presenter on macOS, NFS fallback elsewhere
- add cascade-presenter-webdav to workspace members
- *(mount)* retry NFS mount with admin privileges on macOS
- *(auth)* support user-provided OAuth clients
- *(auth)* add localhost redirect OAuth2 flow and compile-time credentials
- *(init)* add non-interactive flags for scripted setup
- *(mount)* wire NFS presenter into sync runner
- *(cli)* implement cache warm and cache clear commands
- *(cascade)* implement PID file and SIGTERM for cascade stop
- *(cascade)* prompt for type-specific credentials in backend add
- *(cascade)* prompt for S3 credentials in init wizard
- *(cascade)* wire multi-backend dispatcher into cascade start
- *(cascade)* add backend-auth command for gdrive OAuth device-code flow
- *(cli)* add cascade init command with guided setup
- *(ci)* maximise strictness of all quality gates
- add backend-add and backend-remove CLI commands
- sync runner auto-pins new files, mount starts cache manager
- wire pin/unpin/pin-list/cache CLI commands to cache manager
- add cache manager with pinning, eviction, and lifecycle policies
- *(cli)* implement status and backend-list with real state DB queries
- *(nfs)* bridge NFS procedures to VFS tree via NfsContext
- *(engine)* integrate .cascade config filtering into sync runner
- *(cli)* wire start command to NFS server, sync runner, and GDrive backend
- *(cli)* cascade binary with clap commands and integration tests

### Fixed

- *(ci)* replace platform-specific doc link with generic description
- *(ci)* allow missing_const_for_fn on non-macOS unmount_path stub
- *(ci)* gate VfsPresenter and WebDavPresenter imports to macOS only
- *(ci)* gate macOS-only tests behind cfg target_os, fix clippy in presenter-webdav
- *(mount)* resolve nfsv4/webdav conflict — WebDAV → NFS fallback chain on macOS
- *(ci)* suppress missing_const_for_fn and unnecessary_wraps on non-macOS stubs
- *(cli)* suppress dead_code for is_mounted stub on non-macOS
- *(cli)* remove unimplemented backend type claims from help text and display
- *(mount)* gate NFS helpers behind #[cfg(target_os = "macos")]
- *(mount)* guard against double-start using PID file liveness check
- *(init)* register backend in state DB after writing config
- *(backend)* sync config.toml on backend-add and backend-remove
- *(cache)* use config_dir() helper for DB path in open_db()
- *(init)* write gdrive credential file during cascade init
- *(cli)* check PID file liveness in cascade status
- *(cascade)* gate nix dependency and stop() on unix cfg
- *(cascade)* clean up pre-existing clippy errors in test files
- *(cascade)* fail loudly on missing config files and remove unsupported local backend
- *(auth)* platform-guard token storage and redact OAuthConfig debug output
- *(cascade)* resolve clippy errors in CLI modules
- *(cascade)* resolve test compile errors after sync API changes
- resolve reconciliation conflicts and RwLock type mismatch
- *(fuse)* use Generation newtype and commit mount.rs format
- resolve reconciliation conflicts in mount CLI, clippy warnings

### Other

- release v0.1.1
- remove hardcoded toolchain paths from git hooks
- *(engine)* separate sync runner from Engine::start()
- *(cli)* add integration tests for CLI command functions
- fmt after CliContext refactor
- *(cli)* introduce CliContext for shared config paths and verbosity
- apply cargo fmt across workspace
- apply automated clippy fixes across workspace
- fmt fixes from shared compilation CI
- add engine lifecycle and init config integration tests
- *(cli)* use Engine struct in mount command
- add NFS/FUSE presenter integration tests
- add P2P integration, e2e, and property tests
- add 11 integration tests for expression evaluation and providers
- add 8 integration tests for conflict detection and multi-backend VFS
- add 8 integration tests for cache lifecycle
- add integration tests for config, state DB, and VFS routing
- clippy and rustfmt pass across workspace
- add .cascade fixture files for config parser integration tests

## [0.1.1](https://github.com/Mearman/cascade/compare/v0.1.0...v0.1.1) - 2026-05-29

### Added

- *(mount)* add FSKit as primary macOS presenter with proper fallback cleanup
- *(mount)* prefer NFSv4 on macOS, fall back to v3 with escalation
- *(mount)* use WebDAV presenter on macOS, NFS fallback elsewhere
- add cascade-presenter-webdav to workspace members
- *(mount)* retry NFS mount with admin privileges on macOS
- *(auth)* support user-provided OAuth clients
- *(auth)* add localhost redirect OAuth2 flow and compile-time credentials
- *(init)* add non-interactive flags for scripted setup
- *(mount)* wire NFS presenter into sync runner
- *(cli)* implement cache warm and cache clear commands
- *(cascade)* implement PID file and SIGTERM for cascade stop
- *(cascade)* prompt for type-specific credentials in backend add
- *(cascade)* prompt for S3 credentials in init wizard
- *(cascade)* wire multi-backend dispatcher into cascade start
- *(cascade)* add backend-auth command for gdrive OAuth device-code flow
- *(cli)* add cascade init command with guided setup
- *(ci)* maximise strictness of all quality gates
- add backend-add and backend-remove CLI commands
- sync runner auto-pins new files, mount starts cache manager
- wire pin/unpin/pin-list/cache CLI commands to cache manager
- add cache manager with pinning, eviction, and lifecycle policies
- *(cli)* implement status and backend-list with real state DB queries
- *(nfs)* bridge NFS procedures to VFS tree via NfsContext
- *(engine)* integrate .cascade config filtering into sync runner
- *(cli)* wire start command to NFS server, sync runner, and GDrive backend
- *(cli)* cascade binary with clap commands and integration tests

### Fixed

- *(ci)* replace platform-specific doc link with generic description
- *(ci)* allow missing_const_for_fn on non-macOS unmount_path stub
- *(ci)* gate VfsPresenter and WebDavPresenter imports to macOS only
- *(ci)* gate macOS-only tests behind cfg target_os, fix clippy in presenter-webdav
- *(mount)* resolve nfsv4/webdav conflict — WebDAV → NFS fallback chain on macOS
- *(ci)* suppress missing_const_for_fn and unnecessary_wraps on non-macOS stubs
- *(cli)* suppress dead_code for is_mounted stub on non-macOS
- *(cli)* remove unimplemented backend type claims from help text and display
- *(mount)* gate NFS helpers behind #[cfg(target_os = "macos")]
- *(mount)* guard against double-start using PID file liveness check
- *(init)* register backend in state DB after writing config
- *(backend)* sync config.toml on backend-add and backend-remove
- *(cache)* use config_dir() helper for DB path in open_db()
- *(init)* write gdrive credential file during cascade init
- *(cli)* check PID file liveness in cascade status
- *(cascade)* gate nix dependency and stop() on unix cfg
- *(cascade)* clean up pre-existing clippy errors in test files
- *(cascade)* fail loudly on missing config files and remove unsupported local backend
- *(auth)* platform-guard token storage and redact OAuthConfig debug output
- *(cascade)* resolve clippy errors in CLI modules
- *(cascade)* resolve test compile errors after sync API changes
- resolve reconciliation conflicts and RwLock type mismatch
- *(fuse)* use Generation newtype and commit mount.rs format
- resolve reconciliation conflicts in mount CLI, clippy warnings

### Other

- remove hardcoded toolchain paths from git hooks
- *(engine)* separate sync runner from Engine::start()
- *(cli)* add integration tests for CLI command functions
- fmt after CliContext refactor
- *(cli)* introduce CliContext for shared config paths and verbosity
- apply cargo fmt across workspace
- apply automated clippy fixes across workspace
- fmt fixes from shared compilation CI
- add engine lifecycle and init config integration tests
- *(cli)* use Engine struct in mount command
- add NFS/FUSE presenter integration tests
- add P2P integration, e2e, and property tests
- add 11 integration tests for expression evaluation and providers
- add 8 integration tests for conflict detection and multi-backend VFS
- add 8 integration tests for cache lifecycle
- add integration tests for config, state DB, and VFS routing
- clippy and rustfmt pass across workspace
- add .cascade fixture files for config parser integration tests
