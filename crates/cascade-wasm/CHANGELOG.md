# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.128](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.127...cascade-wasm-v0.1.128) - 2026-06-16

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.127 [skip ci]
- release v0.1.126 [skip ci]
- release v0.1.125 [skip ci]
- release v0.1.124 [skip ci]
- release v0.1.123 [skip ci]
- release v0.1.122 [skip ci]
- release v0.1.121 [skip ci]
- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.127](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.126...cascade-wasm-v0.1.127) - 2026-06-16

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.126 [skip ci]
- release v0.1.125 [skip ci]
- release v0.1.124 [skip ci]
- release v0.1.123 [skip ci]
- release v0.1.122 [skip ci]
- release v0.1.121 [skip ci]
- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.126](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.125...cascade-wasm-v0.1.126) - 2026-06-16

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.125 [skip ci]
- release v0.1.124 [skip ci]
- release v0.1.123 [skip ci]
- release v0.1.122 [skip ci]
- release v0.1.121 [skip ci]
- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.125](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.124...cascade-wasm-v0.1.125) - 2026-06-16

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.124 [skip ci]
- release v0.1.123 [skip ci]
- release v0.1.122 [skip ci]
- release v0.1.121 [skip ci]
- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.124](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.123...cascade-wasm-v0.1.124) - 2026-06-16

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.123 [skip ci]
- release v0.1.122 [skip ci]
- release v0.1.121 [skip ci]
- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.123](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.122...cascade-wasm-v0.1.123) - 2026-06-15

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.122 [skip ci]
- release v0.1.121 [skip ci]
- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.122](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.121...cascade-wasm-v0.1.122) - 2026-06-15

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.121 [skip ci]
- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.121](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.120...cascade-wasm-v0.1.121) - 2026-06-15

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.120 [skip ci]
- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.120](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.119...cascade-wasm-v0.1.120) - 2026-06-12

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.119 [skip ci]
- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.119](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.118...cascade-wasm-v0.1.119) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.118 [skip ci]
- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.118](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.117...cascade-wasm-v0.1.118) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.117 [skip ci]
- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.117](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.116...cascade-wasm-v0.1.117) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.116 [skip ci]
- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.116](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.115...cascade-wasm-v0.1.116) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.115 [skip ci]
- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.115](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.114...cascade-wasm-v0.1.115) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.114 [skip ci]
- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.114](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.113...cascade-wasm-v0.1.114) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.113 [skip ci]
- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.113](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.112...cascade-wasm-v0.1.113) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.112 [skip ci]
- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.112](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.111...cascade-wasm-v0.1.112) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.111 [skip ci]
- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.111](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.110...cascade-wasm-v0.1.111) - 2026-06-11

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* add path field to FileEntry literal; document nested-listing follow-up
- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.110 [skip ci]
- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.110](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.109...cascade-wasm-v0.1.110) - 2026-06-08

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.109 [skip ci]
- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.109](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.108...cascade-wasm-v0.1.109) - 2026-06-08

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.108 [skip ci]
- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.108](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.107...cascade-wasm-v0.1.108) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.107 [skip ci]
- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.107](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.106...cascade-wasm-v0.1.107) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.106 [skip ci]
- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.106](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.105...cascade-wasm-v0.1.106) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.105 [skip ci]
- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.105](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.104...cascade-wasm-v0.1.105) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.104 [skip ci]
- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.104](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.103...cascade-wasm-v0.1.104) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.103 [skip ci]
- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.103](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.102...cascade-wasm-v0.1.103) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.102 [skip ci]
- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.102](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.101...cascade-wasm-v0.1.102) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.101 [skip ci]
- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.101](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.100...cascade-wasm-v0.1.101) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.100 [skip ci]
- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.100](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.99...cascade-wasm-v0.1.100) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.99 [skip ci]
- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.99](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.98...cascade-wasm-v0.1.99) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.98 [skip ci]
- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.98](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.97...cascade-wasm-v0.1.98) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.97 [skip ci]
- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.97](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.96...cascade-wasm-v0.1.97) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* exclude cache and sync modules from wasm32 target
- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- *(wasm)* cover the limit= query truncation in the router
- release v0.1.96 [skip ci]
- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.96](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.95...cascade-wasm-v0.1.96) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- *(wasm)* inject engine state into the router and add contract tests
- release v0.1.95 [skip ci]
- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.95](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.94...cascade-wasm-v0.1.95) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.94 [skip ci]
- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.94](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.93...cascade-wasm-v0.1.94) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.93 [skip ci]
- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.93](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.92...cascade-wasm-v0.1.93) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.92 [skip ci]
- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.92](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.91...cascade-wasm-v0.1.92) - 2026-06-07

### Added

- *(wasm)* add file delete mutator for engine storage consistency
- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.91 [skip ci]
- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.91](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.90...cascade-wasm-v0.1.91) - 2026-06-06

### Added

- *(wasm)* wire lifecycle policies, pin deletion, peers, and query params
- *(wasm)* wire POST /v1/backends and POST /v1/pins through engine storage
- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- *(cascade-wasm)* apply cargo fmt
- release v0.1.90 [skip ci]
- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.90](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.89...cascade-wasm-v0.1.90) - 2026-06-06

### Added

- *(web)* wire Google Drive API to populate engine file storage
- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.89 [skip ci]
- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.89](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.88...cascade-wasm-v0.1.89) - 2026-06-06

### Added

- *(wasm)* wire Google Drive auth route and token restore
- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.88 [skip ci]
- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.88](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.87...cascade-wasm-v0.1.88) - 2026-06-06

### Added

- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.87 [skip ci]
- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.87](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.86...cascade-wasm-v0.1.87) - 2026-06-06

### Added

- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.86 [skip ci]
- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.86](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.85...cascade-wasm-v0.1.86) - 2026-06-06

### Added

- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.85 [skip ci]
- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.85](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.84...cascade-wasm-v0.1.85) - 2026-06-06

### Added

- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.84 [skip ci]
- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.84](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.83...cascade-wasm-v0.1.84) - 2026-06-06

### Added

- *(wasm)* wire engine storage through PWA request handlers
- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.83 [skip ci]
- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.83](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.82...cascade-wasm-v0.1.83) - 2026-06-06

### Added

- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Fixed

- *(wasm)* suppress dead_code on EngineState scaffold

### Other

- release v0.1.82 [skip ci]
- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.82](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.81...cascade-wasm-v0.1.82) - 2026-06-06

### Added

- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- release v0.1.81 [skip ci]
- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.81](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.80...cascade-wasm-v0.1.81) - 2026-06-06

### Added

- *(wasm)* wire WASM adapters into cascade-wasm for portable engine construction
- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- release v0.1.80 [skip ci]
- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.80](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.79...cascade-wasm-v0.1.80) - 2026-06-05

### Added

- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- release v0.1.79 [skip ci]
- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.79](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.78...cascade-wasm-v0.1.79) - 2026-06-05

### Added

- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- release v0.1.78 [skip ci]
- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.78](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.77...cascade-wasm-v0.1.78) - 2026-06-05

### Added

- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- release v0.1.77 [skip ci]
- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.77](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.76...cascade-wasm-v0.1.77) - 2026-06-04

### Added

- *(wasm)* wire unified handle_request API and session state
- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- *(wasm)* fix unresolved doc links in cascade-wasm crate root
- release v0.1.76 [skip ci]
- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.76](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.75...cascade-wasm-v0.1.76) - 2026-06-04

### Added

- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- release v0.1.75 [skip ci]
- apply cargo fmt to portable traits and cascade-wasm

## [0.1.75](https://github.com/Mearman/cascade/compare/cascade-wasm-v0.1.74...cascade-wasm-v0.1.75) - 2026-06-04

### Added

- *(wasm)* add cascade-wasm crate proving wasm32-unknown-unknown toolchain

### Other

- apply cargo fmt to portable traits and cascade-wasm
