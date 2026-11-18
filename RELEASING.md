# Releasing

`.github/workflows/release.yml` builds and tests every artifact on each run: imgquant for five
targets, seven abi3 wheels and the sdist, and the npm tarball. It publishes only when it is
started from a `v*` tag with **publish** ticked. Then it creates the GitHub Release (the imgquant
archives, `SHA256SUMS`, build provenance attestations and the notes from `CHANGELOG.md`) and
uploads the wheels and the sdist to PyPI by trusted publishing. crates.io and npm are published
by hand, because both registries need a package to exist before trusted publishing can be set up
for it.

The workflow refuses to publish while `CHANGELOG.md` says "unreleased" or has a `TODO(release)`
comment, while the versions of the packages disagree, or from any ref but the tag `v<version>`.

## 3.0.0, launch day

1. **Verify locally** on the release commit:
   - In `CHANGELOG.md`, change `## 3.0.0 (unreleased)` to `## 3.0.0 (YYYY-MM-DD)` and delete
     the `TODO(release)` comment.
   - In `README.md`, uncomment the CI badge under the other badges.
   - `cargo test --workspace --locked` and `cargo publish --workspace --dry-run --locked`.
   - `wasm/build.sh`, then `node --test wasm/tests/package.test.mjs` and `npm pack --dry-run`
     in `wasm/pkg`.
   - `uvx maturin build --release --locked` and `uvx maturin sdist` in `python/`.
2. **Make the repository public:** Settings > General > Danger zone > Change visibility. Then:
   - Settings > Advanced Security (Code security) > Private vulnerability reporting > Enable,
     so the "Report a vulnerability" button that `SECURITY.md` points to exists.
   - Settings > General > Social preview: upload `docs/great-wave.png`.
   - About: description, topics and the homepage `https://docs.rs/libimgquant`.
3. **Push** the release commit to `main`, once.
4. **Run CI once:** the push starts it. Also run Actions > Release > Run workflow from `main`
   with publish unticked. Every job of both must pass.
5. **Set up publishing:**
   - **Environment:** Settings > Environments > New environment `release`. Under "Deployment
     branches and tags", choose "Selected branches and tags" and add the tag rule `v*`.
   - **PyPI** (an account with 2FA): Account > Publishing > Add a new pending publisher
     (GitHub): project `libimgquant`, owner `kiwina`, repository `libimgquant`, workflow
     `release.yml`, environment `release`. A pending publisher does not reserve the name, so
     publish soon after adding it.
6. **crates.io:** log in with GitHub and verify your email address. Create an API token with
   the scopes `publish-new` and `publish-update` and a short expiry, run `cargo login`, then
   `cargo publish --workspace --locked`. It publishes libimgquant, then imgquant and
   libimgquant-capi. The publish is not atomic: if it stops partway, publish the rest with `-p`.
7. **npm** (an account with 2FA): publish the tarball that the step 4 Release run built and
   tested, not a local build: `gh run download <run id> -n npm`, `npm login`, then
   `npm publish libimgquant-3.0.0.tgz`. An unscoped package is public by default.
8. **Tag:** `git tag -a v3.0.0 -m "libimgquant 3.0.0" && git push origin v3.0.0`.
9. **Release:** Actions > Release > Run workflow > Use workflow from: Tags > `v3.0.0`, with
   publish ticked, or `gh workflow run release.yml --ref v3.0.0 -f publish=true`.
10. **Check:**
    - docs.rs has built libimgquant and libimgquant-capi.
    - `cargo binstall imgquant` downloads the release archive.
    - `pip install libimgquant` works in a fresh venv on Linux, macOS and Windows.
    - `npm view libimgquant` shows the new version.
    - `gh attestation verify imgquant-x86_64-unknown-linux-musl-v3.0.0.tar.gz -R kiwina/libimgquant`
      passes.

## Later releases

1. **Versions:** bump the five `Cargo.toml` files (and the `libimgquant` dependency of `capi/`,
   `cli/`, `python/` and `tools/iqeval/`), `wasm/npm/package.json`, and `LIQ_VERSION`/`LIQ_VERSION_STRING` in
   `capi/assets/libimgquant.h`, `capi/src/lib.rs` and `capi/tests/capi.rs`. Add the dated
   section to `CHANGELOG.md`.
2. **Licenses:** when dependencies change, regenerate `python/THIRD-PARTY-LICENSES.txt` with
   the command in `scripts/third-party-licenses.sh`. CI fails while it is out of date.
3. Steps 1, 3, 4 and 6 to 10 above.

## Optional: trusted publishing for crates.io and npm

Once 3.0.0 exists on both registries, steps 6 and 7 can move into `release.yml`:

- **crates.io:** on each of the three crates, Settings > Trusted Publishing > GitHub: `kiwina`,
  `libimgquant`, `release.yml`, `release`. Then add a job (environment `release`, permission
  `id-token: write`) that runs `rust-lang/crates-io-auth-action@v1` and
  `cargo publish --workspace --locked` with its token.
- **npm:** package Settings > Trusted Publisher > GitHub Actions, with the same four values
  (needs npm 11.5.1 or later; Node 24 has it). Then add a job that downloads the `npm` artifact
  and runs `npm publish` on the tarball; it adds provenance automatically.
