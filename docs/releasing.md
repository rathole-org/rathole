# Releasing rathole

Versioned releases use a GitHub Release as the manual request and a protected
GitHub Actions environment as the publication approval. Pushing a version tag
alone does not publish anything.

The workflow has two independent manual decisions:

1. A maintainer publishes a prepared GitHub Release. This emits the
   `release.published` event and starts validation and staging.
2. After the staged outputs have been reviewed, one required reviewer approves
   the waiting `release` environment deployment.

Publishing the GitHub Release in step 1 makes its tag, title, and notes public.
The workflow does not attach custom archives, publish the crate, or create
versioned container tags until step 2 is approved.

## One-time repository setup

Configure an environment named `release` in **Settings > Environments**:

- required reviewers: `sjtrny`, `yujqiao`, and `fernvenue`;
- any one reviewer may approve;
- **Prevent self-review** disabled;
- administrator bypass disabled;
- deployment tags restricted to `v*`; and
- `CRATES_IO_API_TOKEN` stored as an environment secret.

For the versioned path, the workflow grants `contents: write` only to the
protected publisher. The pre-approval container job has `packages: write`
solely to push an untagged digest for runtime verification; only the protected
publisher creates named version aliases. The separate `dev` release job keeps
its existing `contents: write` permission and cannot run for a release event.

## Request a release

1. Update the package version in `Cargo.toml` and `Cargo.lock`, commit it, and
   let the complete required workflow pass on the exact candidate SHA.
2. Confirm `.github/workflows/publish.yml` is present on the repository's
   default branch and at the candidate SHA.
3. Open **Releases > Draft a new release** in GitHub.
4. Choose or create the exact tag `v<package-version>` at the candidate commit.
   Do not move or reuse a tag.
5. Select **Set as a pre-release** when the SemVer contains a prerelease suffix
   such as `-rc.1`. Leave it unselected for a stable version.
6. Generate or write the title and notes, review the target commit, and choose
   **Publish release**. Saving a draft does not start the workflow.

For the 0.6 cycle:

- request `v0.6.0-rc.1` as a prerelease from its exact `dev` candidate SHA;
- after the RC gates and soak pass, make a version-only `0.6.0` promotion
  commit, fast-forward `main`, and request `v0.6.0` from that exact SHA; and
- do not point the RC and stable tags at the same commit because each tag must
  match the package version in its commit.

GitHub may reject release creation against a non-default target if the actor or
integration cannot modify workflows and the target's `.github/workflows/` tree
differs from the default branch. Synchronize the complete workflow tree first,
or create the release in the authenticated GitHub web UI with an account that
has the required permission.

## Review and approve

1. Open the triggered **Publish** workflow run.
2. Wait for `Validate release input`, all nine `Binary` jobs, `Container`, and
   `Stage release assets` to pass.
3. Review the run summary. It records the channel, tag, full SHA, crate
   checksum, nine archive checksums, and staged multi-platform container
   digest. Review the title and notes on the linked GitHub Release as well.
4. In the waiting `Publish approved release` job, choose **Review
   deployments**, select `release`, and choose **Approve and deploy**. One of
   `sjtrny`, `yujqiao`, or `fernvenue` is sufficient.

The approved job revalidates the still-published release, tag, package version,
source branch, SHA, stable-alias ordering, crate checksum, and archive
checksums. It then performs these writes in order:

1. promote the verified GHCR digest to the version and eligible stable aliases;
2. publish and checksum-verify the crate on crates.io;
3. upload and byte-verify the workflow-managed GitHub Release assets; and
4. set the stable release's latest status when applicable.

Existing release notes and unmanaged assets are preserved. Identical outputs
are reused on a retry; conflicting crate source or release assets fail closed.

## Reject or recover

- Choosing **Reject** fails the protected job. The crate, versioned GHCR tags,
  and custom GitHub assets remain unpublished. The public GitHub Release and
  pre-approval Actions artifacts remain for inspection.
- Correct metadata or source errors with a new commit and version/tag rather
  than moving a published tag.
- For a transient failure after approval, rerun the failed workflow jobs. The
  destination checks make an identical partial publication safe to resume.
- Never use the rolling `dev-latest` draft as a versioned release request. It is
  maintained automatically by pushes to `dev`.
