# Upgrade

Spectra supports updating its own binary from GitHub release assets.

## Commands

```bash
spectra --update
```

- `--update` checks GitHub releases and replaces the current binary if a newer version exists.
- The command exits without starting a terminal session or server.
- `--update` is blocked while any spectra server socket is active.

When a server is active, `spectra --update` prints an error similar to:

```
--update cannot run while a spectra server is active
```

## Release source

- GitHub repository: `aplio/spectra`
- Releases are read from GitHub Releases.

## Supported Platforms

- `linux-x86_64`
- `macos-arm64`

Other OS/arch combinations return an unsupported-platform error.

## Asset naming contract

Release assets must include a tarball with this exact format:

```
spectra-{target}.tar.gz
```

Examples:

- `spectra-linux-x86_64.tar.gz`
- `spectra-macos-arm64.tar.gz`

The archive must include a `spectra` executable.

## Test mode

The e2e tests use a deterministic mock source:

- `SPECTRA_TEST_UPDATE_SOURCE=mock`
- `SPECTRA_TEST_UPDATE_STATE=up_to_date|has_update|error`
