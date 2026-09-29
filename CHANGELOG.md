# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Fixed
### Changed
### Deprecated
### Removed
### Security

- Documented a TOCTOU race: a concurrent process able to modify the walked tree can swap
  a directory for a symlink between the type check and the directory read, making the
  walk escape the root directory. See the README "Security" section.
- Hardened CI/CD supply chain: GitHub Actions pinned to commit SHAs, least-privilege
  workflow permissions, crates.io trusted publishing (OIDC) from a protected `release`
  environment, Dependabot security updates, and actionlint/zizmor workflow linting.

## [2.2.0] - 2026-09-28

### Fixed

- Stack overflow when filtering out many entries, e.g. on tokio worker threads (#13).

### Changed

- Directories filtered with `Filtering::IgnoreDir` are no longer read, so they
  no longer produce IO errors (e.g. permission denied).
- Update dependencies: `futures-lite` 2.6, `async-fs` 2.2.

## [2.1.0] - 2025-01-27

### Added

- New `into_io` and `From<Error> for io::Error` methods.

## [2.0.0] - 2024-06-15

### Added

- New error type that allows to get the path in the filesystem where the error occured.

### Changed

- Improved CI/CD with updated github actions and automated release notes.

## [1.0.0] - 2024-01-04

### Changed

- Migration to Rust edition 2021.
- Stabilize API to 1.0.0.

## [0.2.0] - 2020-09-07

### Changed

- Update to futures-lite 1.2.

### Fixed

- Docs typo.

## [0.1.0] - 2020-08-31

### Added

- Initial release