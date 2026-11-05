# CI integration for the suite

GitHub only runs workflows from `.github/workflows/` at the **repository root**, so the Touch
Keyer build is not wired up by anything inside this `touch-keyer/` folder on its own. This
directory stages the one change you make at the suite root when you merge.

## What to do when merging into `w1ve-keyer-suite`

Replace the repo-root workflow with the merged one here:

```
cp touch-keyer/ci/release-esp32.yml .github/workflows/release-esp32.yml
```

`ci/release-esp32.yml` is the existing `Release ESP32 Player firmware` workflow **extended** to also
build the Touch Keyer. It is a superset — the Player jobs, artifact names (`esp32-*`), release
assets, and the root web-installer layout are unchanged — so existing behavior is preserved. It
adds:

- a `build-touch-keyer` job (esp32c6, 8 MB, in `touch-keyer/`),
- the Touch Keyer merged image + docs as release assets,
- the Touch Keyer installer under `/touch-keyer/` in the **single** Pages deploy (so the two
  installers never race the one Pages site), and
- `touch-keyer/docs/release-notes.md` as the release-notes body (`append_body: true`).

Nothing else at the suite root changes. The Cargo workspace is untouched (neither ESP32 project is
a workspace member), and the trigger is still a `v*` tag.

After copying, commit both the new `touch-keyer/` tree and the updated
`.github/workflows/release-esp32.yml`, then cut a `v*` tag to produce a release with both
firmwares and both installers.
