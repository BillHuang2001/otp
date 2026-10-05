# `.github/scripts/` — CI/CD helper scripts

## Intent
Executable helpers invoked by `.github/workflows/*` and by the composite actions in `.github/actions/`.
Three languages, no build step — scripts run straight from a checkout:
POSIX `sh`/`bash` (`*.sh`), JavaScript (`pr-comment.js`, required by `actions/github-script`), and Erlang escripts (`*.es`, executed by `escript` thanks to their `#!/usr/bin/env escript` shebang).

## API Surface (one line per script)

### Changed-path filters (stdout-only YAML generators)
- `path-filters.sh` — prints `emulator`/`debug`/`epmd` keys plus one key per `lib/*/doc` app, with YAML anchors.
- `c-code-path-filters.sh` — prints `c-files:` plus every app that actually has C/C++/autotools sources (ignoring `test/` and `examples/`).

### Version / branch metadata
- `get-major-versions.sh` — `curl`s `otp_versions.table` from `master` on GitHub and prints OTP major numbers.
- `get-supported-versions.sh` — first 3 lines of `get-major-versions.sh` (the supported majors).
- `get-supported-branches.sh` — prints a JSON array `["master","maint","maint-N",…]`.
- `update-gh-actions-versions.es` — maintainer tool (no CI caller): approves/merges open `renovate/*github-actions*` PRs, forward-merges maint→master, rewrites `renovate.json5` `baseBranches`.

### Base image / Docker / prebuilt cache
- `build-base-image.sh BASE_BRANCH [DOCKERFILE_SUFFIX]` — maps branch/tag → `maint-N`, derives the base tag from `.github/dockerfiles/Dockerfile.<suffix>`, loads/builds/tags the `otp` base image, optionally `docker save`s it as `otp_docker_base.tar`.
- `cleanup_gh_runner.sh` — frees runner disk (removes dotnet/ghc/boost/`$AGENT_TOOLSDIRECTORY`, `docker system prune -a -f`); needs `sudo` + Docker.
- `restore-from-prebuilt.sh CACHE_SOURCE_DIR TARGET [ARCHIVE]` — merges a fresh `git archive` with cached `otp_src.tar.gz`/`otp_cache.tar.gz`, pruning cache entries invalidated by the change, and writes the unified tar to `TARGET`.
- `init-pre-release.sh [TARGET] [CACHE_TAR]` — `git archive HEAD` into a pre-release tar; bootstraps generated `configure` files on old branches; optionally copies mtimes from a cache tar for byte-identical files.
- `create-artifacts.sh DIR TAG` — repacks `otp_src.tar.gz` as `otp_src_<vsn>.tar.gz` and moves `otp_doc_{html,man}.tar.gz` into `DIR`.

### macOS / iOS build helpers
- `build-macos.sh [build_docs] <configure args>` — sets `MAKEFLAGS`/`ERL_TOP`/`ERLC_USE_SERVER`/`RELEASE_ROOT`, runs `./otp_build configure|boot -a|release -a`, optionally `make release_docs DOC_TARGETS=chunks`.
- `build-macos-wxwidgets.sh` — downloads and builds wxWidgets (`$WXWIDGETS_VERSION`, fallback `3.2.8.1`) with cocoa into `./wxWidgets/release`.

### Documentation / release publishing
- `release-docs.sh RELEASE VSN` — builds docs inside the `otp` docker image (different command for `< 27` vs `>= 27`; 27.0 uses `DOC_TARGETS=html`) and produces `otp_doc_man.tar.gz` / `otp_doc_html.tar.gz`.
- `sync-github-releases.sh [REPO] [TOKEN] [RELEASE_FILTER] [TIME_LIMIT]` — rsyncs artifacts from `erlang.org/download` and publishes them to the matching GitHub release/tag.
- `renovate-vendored-deps.sh REMOTE BASE_REF HEAD_REF` — re-runs each `update` script named in `vendor.info` files touched by a PR, commits the result and pushes back to the PR branch (unless `REMOTE` = `skip`).

### SBOM / SPDX / VEX / compliance
- `otp-compliance.es` — SPDX SBOM repair + validation and OpenVEX statement generation (largest script; see below).
- `ort-scanner.es` — replacement for ORT's built-in scanner (`init`, `restore-cache`, `scan -t scancode|reuse -s overwrite|fill`).
- `create-openvex-pr.sh REPO BRANCH` — force-pushes `BRANCH` and opens/updates a PR against the orphan `openvex` branch; called only by `otp-compliance.es vex verify -p`.
- `add-vex-license.sh [FILE_LIST]` — creates `<file>.sigstore.license` (Apache-2.0, current year) for each path in `FILE_LIST` (default `/tmp/changed-vex.txt`).

### GitHub metadata / PR comments
- `get-pr-number.es ORG/REPO HEAD_SHA` — finds the open PR whose head SHA matches via `gh api` and writes `result=<number>` (or empty) to `$GITHUB_OUTPUT`.
- `pr-comment.js` — CommonJS module `({github, context, state, pr_number}) => …`; creates/updates the `## CT Test Results` PR comment with CT-log / HTML-doc / Windows-installer links and a `<!-- marker -->` sentinel.

### Compiler-hardening SARIF
- `ossf-sarif-generator.es '<compiler-options.json>'` — compares OSSF recommended hardening options against `erlang:system_info(compile_info)` plus `$_CFLAGS`/`$_LDFLAGS` and prints a SARIF 2.1.0 report of missing flags.

## Large scripts — internal structure

### `otp-compliance.es` (≈3940 lines / 240 KB)
`argparse` CLI with four command groups:
- `sbom otp-info|test-file|vendor|osv-scan`
- `vex init|run|verify|test`
- `explore classify-license|classify-license-copyright|diff`
- `compliance detect|check`

Approximate layout:
- ~40–135 macros: SPDX constants, VEX paths (`vex/`, `openvex.table`), `?GH_ACCOUNT = "erlang/otp"`, GH advisory pagination options.
- ~228–520 CLI spec + option constructors (`input_option`, `sbom_option`, `apply_excludes`, `apply_curations`, `branch_option`, `create_pr`).
- ~520–1080 SBOM fixups (`sbom_otp/1`, `improve_sbom_with_info/2`, `fix_beam_licenses`, `fix_*`), `explore` classify/diff, `compliance` detect/check, JSON decode helpers.
- ~1080–1500 SPDX package & relationship generation driven by `lib/*/src/*.app.src`, `vendor.info` and `otp_versions.table`.
- ~1528–1900 `osv_scan/1` (POSTs a vendor query batch to `https://api.osv.dev/v1/querybatch` via `httpc`), GitHub issue create/update for vendor CVEs, OpenVEX download, vendor version-range logic.
- ~1920–2200 SPDX build/test/system-doc packages, verification codes, `sbom test-file` plus the optional `ntia-checker` hook.
- ~2200–2800 the `test_*` self-check suite (several functions are exported so they can be called individually).
- ~2820–2960 `init_openvex`/`run_openvex`/`verify_openvex` entry points, GH advisory pagination and branch discovery.
- ~2960–3380 advisory → OpenVEX statement calculation and `vexctl add …` command formatting.
- ~3376–3580 OTP version-tree helpers backed by `otp_versions.table`.
- ~3591–3941 `vex test` fixtures and `with_otp_versions_table/1`, which swaps in an embedded ~100-line `otp_versions.table` snapshot (lines ~3833–3935), runs the test, and restores the real file. That fixture is most of the file's byte size.

### `ort-scanner.es` (≈580 lines)
`init` builds a `scan-result.json` skeleton from `analyze-result.json` (validates the git SHA, computes `sha1sum` per file); `restore-cache` reuses licence results for files whose SHA1 is unchanged; `scan` fills or overwrites results using `scancode` or `reuse`.

### `sync-github-releases.sh` (≈380 lines)
Polls `erlang.org` and syncs releases. Structure: curl/markdown helpers (`_curl_get/post/patch`, `_json_escape`, `_strip_name`, `_format_markdown`), tag pagination (`_next_page`), an asset diff that fills `RI`/`ALL_TAGS`/`CREATE_RELEASE`/`UPDATE_BODY`, one `rsync` guarded by `timeout`, release creation/body updates, `_upload_artifacts`, then a fallback that clones one tag and rebuilds its prebuild + docs in Docker (`init-pre-release.sh` → `build-base-image.sh` → `Dockerfile.64-bit` → `build-otp-tar` → `release-docs.sh` → `create-artifacts.sh`).

## Constraints
- Every script starts with the `%CopyrightBegin%` … `%CopyrightEnd%` block containing `SPDX-License-Identifier: Apache-2.0` and `Copyright Ericsson AB <years>. All Rights Reserved.` followed by the Apache-2.0 text; the comment leader is language-specific (`##` for sh/bash, `%%` for escript, `//` for JS). Enforced on every PR/push by `scripts/license-header.es ci` (`../workflows/license-scanner.yaml`); `*.license` files are exempt from scanning. `.github/**` is not excluded from that scan.
- Shell strictness varies per script: `set -e` (build-macos, init-pre-release, create-openvex-pr, sync-github-releases), `set -eo pipefail` (build-base-image), `set -euo pipefail` (add-vex-license), `set -xe` (restore-from-prebuilt), `set -x` (renovate-vendored-deps), none at all (path-filters, c-code-path-filters, get-*, create-artifacts, release-docs). Match the local file when editing.
- Scripts assume repository root as CWD (`ERL_TOP`); the ones needing it export `ERL_TOP=$(pwd)` themselves (`build-macos.sh`, `renovate-vendored-deps.sh`). `update-gh-actions-versions.es` fails fast unless `.github/scripts/update-gh-actions-versions.es` is readable from CWD.
- Escripts rely on OTP-27+ features (`~"..."` sigils, `json`, `json:format/1`, `argparse`, `maybe`); CI runs them on OTP 27/28 via `erlef/setup-beam` or inside the `otp` Docker image. `update-gh-actions-versions.es` hard-requires ≥ 28.
- No script writes to `$GITHUB_ENV`; the only GitHub plumbing is `$GITHUB_OUTPUT` (`build-base-image.sh`, `get-pr-number.es`).
- `otp-compliance.es` and `ort-scanner.es` must run from the OTP repo root (they read `OTP_VERSION`, `otp_versions.table`, `lib/*/src/*.app.src`).

## Design Decisions
- Path filters are generated instead of committed so newly added OTP apps are picked up by CI without touching the workflow: `reusable-pack.yaml` redirects both generators into this directory immediately before `dorny/paths-filter` reads them.
- `sync-github-releases.sh` polls on a schedule rather than reacting to tag pushes, because a pushed tag does not guarantee the corresponding artifacts exist on `erlang.org` yet.
- `restore-from-prebuilt.sh` deletes rather than invalidates: instead of tracking dependencies it removes cached outputs when a bootstrap app, parse transform, `yecc`, `asn1` or doc-tool source changed, iterating the `rsync` diff up to 10 times (then drops the whole cache) so second-order invalidations are caught.
- `otp-compliance.es` keeps an embedded `otp_versions.table` fixture so `vex test` can assert exact `vexctl` output without depending on the repo's current version table.

## Known Issues
- `cleanup_gh_runner.sh` line 23 is `set -euo pipefail {0}` — a leftover `shell:` template placeholder; `{0}` becomes a positional parameter. Harmless but should be `set -euo pipefail`.
- `get-pr-number.es` line 2 is `%%! -pa jsx/_build/default/lib/jsx/ebin/` although the script uses the OTP `json` module; the path is stale and does not exist in the repo.
- `sync-github-releases.sh` ~line 370 iterates `dowloads/*` (typo for `downloads/`), so the "delete artifacts we should not upload" filter never matches and that loop is a no-op.
- `get-major-versions.sh` / `get-supported-versions.sh` fetch `otp_versions.table` from `master` over the network, so they depend on GitHub availability and on `master` being the source of truth for supported majors.
- Both path-filter generators enumerate apps via `ls -d lib/*/doc`, so an app without `doc/` is absent from the filters (and from the `reusable-pack.yaml` app matrix).
- `get-supported-versions.sh` depends on `get-major-versions.sh` by a fixed relative path, so it only works when CWD is the repository root.

## Notes for Agents
- This directory is a leaf: no subdirectories, no local test harness. Validate shell edits with `bash -n`; run any `.es` file with no args to get its `argparse` help.
- `path-filters.yaml` and `c-code-path-filters.yaml` do not exist in a clean checkout; they are generated, untracked and NOT git-ignored. Do not treat their absence as a bug and do not `git add` them.
- Most security scripts run inside the `otp` Docker image built from `.github/dockerfiles/Dockerfile.*` plus a built OTP tree, so `escript` there is the image's OTP, not the runner's.
- `pr-comment.js` is a library, not a CLI: it is `require`d and called by `actions/github-script`, so its exported function signature is a de-facto API.

## Dependencies
Beyond the package manager: `git`, `docker` (+ BuildKit), `curl`, `jq`, `python3`, `rsync`, `timeout`, `mdformat` (pinned in `.github/requirements/mdformat.txt`), `gh` CLI with auth, `escript`/OTP 27+, and for `sbom test-file --ntia-checker` the `sbomcheck`/`ntia-conformance-checker` tools.
Network endpoints contacted: `api.github.com`, `api.osv.dev`, `raw.githubusercontent.com`, `github.com` (release download, `nightly.link` links), and the `erlang.org::erlang-download` rsync module.

## Routing Table
Consumers live outside this node: `../workflows/`, `../actions/`, `../dockerfiles/` and the repo-root `scripts/` are read-only cross-references — escalate edits there to `./.github`.
- `path-filters.sh`, `c-code-path-filters.sh` → `../workflows/reusable-pack.yaml` (write `path-filters.yaml` / `c-code-path-filters.yaml` here, then `dorny/paths-filter`)
- `init-pre-release.sh`, `restore-from-prebuilt.sh` → `../workflows/reusable-pack.yaml`, `../actions/build-base-image/action.yaml`
- `build-base-image.sh`, `cleanup_gh_runner.sh` → `../actions/build-base-image/action.yaml`, `../workflows/update-base.yaml`
- `build-macos.sh` → `../workflows/reusable-build-macos.yaml`, `../workflows/reusable-build-ios.yaml`; `build-macos-wxwidgets.sh` → `../workflows/reusable-build-macos.yaml`
- `create-artifacts.sh` → `../workflows/reusable-release.yaml`; `release-docs.sh` → only `sync-github-releases.sh`
- `sync-github-releases.sh` → `../workflows/sync-github-releases.yaml` (also chains `init-pre-release.sh`, `build-base-image.sh`, `release-docs.sh`, `create-artifacts.sh`)
- `otp-compliance.es` → `../workflows/reusable-sbom.yaml`, `../workflows/openvex-sync.yml`, `../workflows/reusable-vendor-vulnerability-scanner.yml`
- `ort-scanner.es` → `../workflows/reusable-sbom.yaml`
- `create-openvex-pr.sh` → invoked only by `otp-compliance.es vex verify -p`
- `add-vex-license.sh` → `../workflows/reusable-vex-attest.yml`
- `renovate-vendored-deps.sh` → `../workflows/renovate-vendored-deps.yaml`
- `get-pr-number.es`, `pr-comment.js` → `../workflows/pr-comment.yaml`
- `ossf-sarif-generator.es` → `../actions/ossf-compiler-flags-scanner/action.yaml`
- `get-supported-branches.sh` → `../workflows/osv-scanner-scheduled.yml`, `otp-compliance.es`; `get-supported-versions.sh` → `get-supported-branches.sh`, `update-gh-actions-versions.es`, `../workflows/osv-scanner-scheduled.yml`
- `update-gh-actions-versions.es` → no CI caller; maintainer-only, run from the repo root
- Base-image recipes these scripts build from → `../dockerfiles/` (read-only)
- Repo-root `scripts/` is a different, unrelated directory: workflow paths like `/github/scripts/run-dialyzer` or `/github/scripts/otp_check_html_links.exs` refer to it, not to this node

## Status
Complete — all 23 files analysed and mapped to their consumers.
