# Contributing

## Build

```bash
cargo build --release
```

The Arch package builds with `makepkg` against the included `PKGBUILD`.

## Tests and Gates

Every change must pass, in this order:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three run in CI; a red gate is not merged.

## Commits and Pull Requests

- Conventional commits (`feat:`, `fix:`, `refactor:`...), one logical change
  per commit, subject in imperative mood.
- PRs target `main` and merge as squash.
- Code, comments and commit messages are in English.

## Translations

The UI is bilingual English/Spanish through the built-in catalog in
`src/i18n`. Every new translatable string needs its Spanish entry; a
coverage test fails the build otherwise. The Spanish catalog must stay
sorted (byte order) or lookups silently miss strings.

## Version Bumps

The version lives in `Cargo.toml`; a bump also touches `Cargo.lock`,
`PKGBUILD`, `version.json`, the README badges and
`data/io.github.gnacho.nextsync.metainfo.xml` (`<releases>`). Keep them in
sync in the same commit.
