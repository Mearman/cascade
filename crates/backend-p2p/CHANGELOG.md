# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.17](https://github.com/Mearman/cascade/compare/cascade-backend-p2p-v0.1.16...cascade-backend-p2p-v0.1.17) - 2026-05-31

### Added

- *(p2p)* wire peer sync — Index exchange, broadcast, block fetch on miss
- *(p2p)* add cascade-backend-p2p crate with content-addressed storage

### Fixed

- *(p2p)* drop intra-doc link to Backend::download
- *(p2p)* silence clippy::duration_suboptimal_units on POLL_INTERVAL

### Other

- cargo fmt for download peer-fetch closure
