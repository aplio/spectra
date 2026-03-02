# contributing

run these before opening a PR:

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

commit message format:

```text
${genre}: ${summary}

reason
why
```

genres:
- feat
- bugfix
- docs
- refactor
- test
- chore
