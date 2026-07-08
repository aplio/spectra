# Upgrade

Spectra supports updating its own binary from GitHub release assets.

## Commands

```bash
spectra --update
```

- `--update` checks GitHub releases and replaces the current binary if a newer version exists.
- The command exits without starting a terminal session or server.
- Updating while a server runs is safe: the swap replaces the file, the
  running server keeps its old inode.

When a server is active and a new binary was installed, `--update`
automatically attempts a live handoff (`spectra server-handoff`) to move the
running server onto the new binary without killing panes. The server refuses
the handoff while any client is attached; in that case `--update` prints the
refusal and a hint to rerun `spectra server-handoff` manually after
detaching all clients.

Each pane carries its last `[pane] handoff_replay_bytes` (default 256 KiB)
of raw output across the handoff; scrollback older than that tail does not
survive.

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
