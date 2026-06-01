# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
