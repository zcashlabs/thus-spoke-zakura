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
   gh attestation verify thus-spoke-zakura-x86_64-unknown-linux-gnu.tar.gz \
     --repo zcashlabs/thus-spoke-zakura
   ```

6. Publish the draft. The publish workflow promotes the immutable versioned
   manifests to the convenience `latest` tags without rebuilding them, then
   tests the public installer without GHCR credentials.

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
zakuracore/zakura:1.2.0
```

`thus-spoke-zakura pull` fetches that set. `start` has no network-side image
resolution and never consumes `latest`. Contributor builds deliberately use
the same exact local tags, allowing a source build to replace the matching
images without changing runtime behavior.
