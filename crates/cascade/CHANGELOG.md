# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
