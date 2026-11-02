# Releasing

Everything is driven by a git tag that matches the version in `Cargo.toml`.

```sh
# 1. bump the version
sed -i 's/^version = ".*"/version = "0.2.0"/' Cargo.toml
cargo build            # refreshes Cargo.lock
git commit -am "Release 0.2.0" && git push
# 2. tag it
git tag v0.2.0 && git push origin v0.2.0
```

The `Release` workflow (`.github/workflows/release.yml`) then:

1. Builds static musl binaries for x86_64 and aarch64 on native runners.
2. Packages each as a tarball, a `.deb` (`cargo deb`) and an `.rpm`
   (`cargo generate-rpm`), using the metadata in `Cargo.toml`.
3. Writes `SHA256SUMS`, creates the GitHub release with auto-generated notes,
   and attaches everything.
4. Renders `packaging/homebrew/mousetouch.rb.in` with the version
   and checksums, attaches it, and pushes it to
   `github.com/Devace-93/homebrew-tap` **if** the repository secret
   `HOMEBREW_TAP_TOKEN` exists (a fine-grained personal access token with
   *Contents: read and write* on the tap repository). Without the secret the
   formula is only attached to the release and can be copied by hand.

## Channels and what they need

| Channel | Updated by | One-time setup |
|---|---|---|
| `get.sh` (curl installer) | nothing: it always resolves the latest release | none |
| `.deb` / `.rpm` / tarballs | the workflow | none |
| Homebrew tap | the workflow (with the secret) | create the `HOMEBREW_TAP_TOKEN` secret |
| crates.io (`cargo install`) | `cargo publish` by hand (`cargo login` once with a crates.io API token) | claim the crate name with the first publish |
| AUR (`mousetouch-git`) | nothing after the first push: it builds from `main` | an AUR account with an SSH key; push `packaging/aur/*` to `ssh://aur@aur.archlinux.org/mousetouch-git.git` (regenerate `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO` after editing the PKGBUILD) |
| Fedora COPR | rebuild the project from `packaging/rpm/mousetouch.spec` on each tag (COPR can watch the repository's tags via its "custom" or "SCM" source) | a COPR project with *internet access during builds* enabled, since `cargo build` downloads crates |
| Nix flake | nothing: `flake.nix` builds from the repository (keep `flake.lock` committed, `nix build github:...` refuses to run without it) | none |
| Install instructions (site + README) | nothing: the Release workflow runs `scripts/set-release-version.py` and commits the new version to `main` | none (run it by hand if the push to `main` is rejected) |
| Landing page (`site/`, <https://mouse.3m4.net>) | the `Pages` workflow on every push to `main` that touches `site/` (or run it by hand from any branch) | GitHub Pages source set to *GitHub Actions* with custom domain `mouse.3m4.net`; a DNS `CNAME mouse -> devace-93.github.io` at the registrar |

## Checking a release locally

```sh
cargo build --release --target x86_64-unknown-linux-musl   # static binary
cargo deb                                                   # target/debian/*.deb
cargo generate-rpm                                          # target/generate-rpm/*.rpm
cargo publish --dry-run                                     # what crates.io would receive
MOUSETOUCH_DRY_RUN=1 MOUSETOUCH_VERSION=v0.1.0 sh get.sh                  # installer, no changes
```
