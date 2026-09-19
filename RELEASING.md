# Releasing Thus Spoke Zakura

## One-time repository setup

The canonical `zcashlabs/thus-spoke-zakura` repository must be public. In the
repository Actions settings, permit GitHub Actions to create releases and write
packages. After their first publication, make both GHCR packages public:

- `ghcr.io/zcashlabs/thus-spoke-zakura-app`
- `ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd`

Keep tag protection or a ruleset for `v*`, and require the normal CI checks on
`main`. These visibility and protection settings are intentionally manual;
workflows must not weaken repository policy.

## Prepare a release

1. Update `[workspace.package].version` in `Cargo.toml` and commit it through a
   reviewed pull request.
2. Run the `Release candidate` workflow manually. This is a dry run: it tests
   and builds every launcher and image target but pushes nothing.
3. From the final commit on `main`, create and push the matching signed or
   annotated tag, for example `v0.1.0`. The workflow rejects a tag that does not
   exactly match the Cargo version.
4. Inspect the generated draft release. Confirm all four launcher archives and
   the installer are listed in `SHA256SUMS`, and inspect both versioned multi-architecture
   image manifests.
5. Verify an artifact attestation when desired:

   ```console
   gh attestation verify ths-x86_64-unknown-linux-gnu.tar.gz \
     --repo zcashlabs/thus-spoke-zakura
   ```

6. Publish the draft. The publish workflow promotes the immutable versioned
   manifests to the convenience `latest` tags without rebuilding them, then
   tests the public installer without GHCR credentials.

### Multi-platform image builds

Release images are built as independent native-platform jobs. AMD64 builds run
on `ubuntu-24.04`, while ARM64 builds run on `ubuntu-24.04-arm`; release-mode
Rust compilation must not run through QEMU. Each job pushes an immutable image
digest, and the workflow creates the versioned multi-platform manifest only
after both architecture jobs succeed. Architecture tags use the form
`<version>-amd64` and `<version>-arm64`; the assembly job resolves these to
immutable digests before creating the public `<version>` tag. The combined manifest is then verified
for `linux/amd64` and `linux/arm64` and receives a build-provenance attestation.

BuildKit layers are persisted with the GitHub Actions cache, scoped by image
and architecture. This keeps incompatible architectures isolated while letting
later release candidates reuse unchanged dependency and build layers. A manual
workflow dispatch follows the same native build matrix and populates the cache,
but does not push digests or create manifests.

For comparison, the `v0.2.0` app-image job took 75 minutes 12 seconds when its
ARM64 Rust release build ran under QEMU. The first uncached native dry run took
7 minutes 40 seconds for AMD64 and 7 minutes 34 seconds for ARM64, reducing the
app-image critical path by about 90%. Cached runs should be recorded in issue
#29 as further releases exercise the persistent caches.

Do not delete and recreate a released tag. A correction gets a new patch
version so the binary and its exact-version runtime images remain an auditable
set.

Official launcher artifacts are compiled with the `release-distribution`
feature. That feature authorizes self-replacement; ordinary Cargo builds expose
non-mutating update checks but cannot overwrite themselves.

## Image contract

The launcher always selects these exact tags using its compiled Cargo version:

```text
ghcr.io/zcashlabs/thus-spoke-zakura-app:<version>
ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd:<version>
zakuracore/zakura:1.4.0
```

`ths pull` fetches that set. `start` has no network-side image
resolution and never consumes `latest`. Contributor builds deliberately use
the same exact local tags, allowing a source build to replace the matching
images without changing runtime behavior.
