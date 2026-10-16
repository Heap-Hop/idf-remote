# Releasing idf-remote

The crates.io package provides the desktop CLI and shared library. Android builds
use this repository's `android-usb` feature and pinned transport fork; they are
not part of the registry package. Selecting a Cargo target or disabling features
does not remove Git dependencies from the published manifest.

## Prepare and verify

Update the root package version and both Cargo lockfiles, run the repository
checks, and commit the intended release sources. With Python 3.11+ and Cargo:

```sh
python3 scripts/prepare_release.py --output .artifacts/release-VERSION
cargo publish --dry-run --locked --manifest-path .artifacts/release-VERSION/source/Cargo.toml
```

The output directory must be new. The script copies tracked working-tree files,
excluding the Android app and CI configuration, into an isolated source tree.
It removes `android-usb`, `nusb`, and the Git transport dependency only from that
copy, then reconciles its lockfile without upgrading pinned registry packages.
Shared Rust sources remain unchanged. The registry manifest recognizes disabled
Android cfgs without advertising an unusable feature.

`release.json` records the source commit, tracked-change status, version, and
manifest/lockfile hashes. Review it and the generated `.crate` file list. Prepare
again after committing any changes so the release records a clean source commit.
The script never uploads a package or edits the repository's Android settings.
Use `--offline` only when all dependency sources are cached.

CI checks desktop packaging independently from Android compilation and tests.
For a release, also verify the staged package with `--all-features`; this enables
only the published desktop feature set.

## Publish

After current-commit CI and the dry run pass, authenticate Cargo and publish the
same prepared source tree:

```sh
cargo publish --locked --manifest-path .artifacts/release-VERSION/source/Cargo.toml
```

Publish from the prepared manifest, not the repository root. Keep the prepared
source and `release.json` with local release records. The repository remains the
source for Android builds, including its Git transport dependency.
