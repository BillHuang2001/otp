# GitHub Actions Workflows

## Intent
CI/CD definition for Erlang/OTP.
`main.yaml` is the single orchestrator: it triggers on `push`, `pull_request` and a nightly `schedule` (cron `0 1 * * *`), computes what changed, builds one 64-bit base, then fans out to per-platform builds, tests, documentation, static analysis, SBOM and (for `OTP-*` tags) release.
The `reusable-*.y*ml` files in this directory are not standalone entry points — they are `workflow_call` targets consumed by `main.yaml` (and, for the vendor scanner, by `workflow_dispatch`).
The remaining files are independently triggered security / maintenance / publishing jobs.
Everything lives in this one flat directory (no subdirectories), so this CONTEXT.md is the complete node — there is no per-workflow child context.

## API Surface

### Orchestrator
- `main.yaml` — name "Build and check Erlang/OTP". Default permissions `contents: read`, `pull-requests: read`.
  - Concurrency group `${{ github.workflow }}-${{ github.ref }}-${{ github.event.pull_request.number || github.sha }}` with `cancel-in-progress: true`.
  - Jobs it calls with `uses:` (18 workflows): `reusable-check-files.yaml`, `reusable-setup.yaml`, `reusable-pack.yaml`, `reusable-build-macos.yaml`, `reusable-build-ios.yaml`, `reusable-build-windows.yaml`, `reusable-build-flavours.yaml`, `reusable-vendor-vulnerability-scanner.yml`, `reusable-build-otp.yaml`, `reusable-freebsd.yaml`, `reusable-openbsd.yaml`, `reusable-solaris.yaml`, `reusable-documentation.yaml`, `reusable-static-analysis.yaml`, `reusable-test.yaml`, `reusable-system-test.yaml`, `reusable-sbom.yaml`, `reusable-release.yaml`.
  - Local jobs: `event_file` (uploads `${{ github.event_path }}` as artifact "Event File"), `zizmor` (runs `zizmorcore/zizmor-action`, uses `.github/zizmor.yml`), `ruleset-checker` (`if: always()`, `needs` all important jobs, reads `toJSON(needs)` and fails only on `failure`/`cancelled` — a *skipped* job counts as OK so conditional jobs don't break GitHub rulesets).
  - DAG / gating: `check-files` only on `pull_request`; `setup` → `pack`; all build jobs gated on `needs.pack.outputs.build-c-code == 'true'` (`build-windows` additionally runs on pushes to `erlang/otp`); `test` gated on `needs.pack.outputs.changes != '[]'`; `system-test` runs with `!cancelled() && needs.pack.result == 'success'` so it still runs after a failed `test`; `release` requires `startsWith(github.ref, 'refs/tags/OTP-') && github.repository == 'erlang/otp'`.

### Reusable workflow contract (`workflow_call`)
- `reusable-setup.yaml` — inputs: none. Outputs: `base_branch`, `full_build_and_check`, `otp_sbom_version`, `docs_base_url`, `tag`, `vsn`.
  - `base_branch` = `github.base_ref` on PRs else `github.ref_name`; `full_build_and_check` = `'true'` on `schedule` or when a PR carries the `full-build-and-check` label; `otp_sbom_version` = tag name on tags else SHA; `tag`/`vsn` = `github.ref` stripped of `refs/tags/` and `OTP-`.
  - Also computes `is_tag_push` and `repository` internally but does not expose them.
  - All event context is passed to the shell through `env:` because `github.event.pull_request.head.ref` is untrusted.
- `reusable-pack.yaml` — inputs: `base_branch` (string), `full_build_and_check` (`'true'|'false'` as string), `bypass_erlang_repo` (boolean, for fork testing).
  - Outputs: `changes` (JSON array string of changed apps), `build-c-code` (`'true'|'false'`), `base_branch` (echoed back through the workflow output because GitHub drops outputs of other jobs).
  - Produces artifacts `otp_git_archive` (`otp_archive.tar.gz`) and `otp_prebuilt` (`otp_src.tar.gz`, `otp_cache.tar.gz`) — these are the shared inputs of nearly every other build/test job.
  - Uses `dorny/paths-filter` three times: changed apps, changed C code, and a "can we reuse the prebuilt cache?" filter (`no-cache`, `deleted`, `bootstrap`, `configure`).
  - `bypass_erlang_repo` is only consumed by the job-level `if`; `main.yaml` always passes `false`.
  - The `pack` job also defines a job-level `all` output (all apps) that is not exposed at workflow level and is currently unused by callers.
- `reusable-check-files.yaml` — inputs `pr_base_sha`, `pr_head_sha`, `pr_author`, `repository`; secret `token` (required).
  - Fails PRs on whitespace errors (`git diff --check` with `core.whitespace tab-in-indent,-trailing-space` over `*.erl *.hrl *.c *.h *.cpp *.hpp`) and on modified `*.beam` files unless the author's repo role is `admin`/`maintain`/`Security Master` (only when `repository == 'erlang/otp'`).
- `reusable-build-macos.yaml` — input `base_branch`. Matrix `macos-15`/arm64 and `macos-15-intel`/x86-64, `fail-fast: false`; builds/caches wxWidgets 3.2.9 (cache key includes a Homebrew dependency fingerprint) then compiles with `.github/scripts/build-macos.sh build_docs --disable-dynamic-ssl-lib --with-dynamic-trace=dtrace`, installs and smoke-tests `erl`/`crypto`/`wx`, and packages a tarball. Artifact `otp_prebuilt_macos_<arch>` (`arm64`, `x86-64`).
- `reusable-build-ios.yaml` — input `base_branch`. Runs on `macos-15`; `RELEASE_LIBBEAM=yes`, xcomp conf `xcomp/erl-xcomp-arm64-ios.conf`, `--without-ssl`, archives `liberlang.a` into an `.xcframework`. Artifact `ios_framework_aarch64-apple-ios`.
- `reusable-build-windows.yaml` — input `base_branch`. Runs on `windows-2025` but all `run:` steps default to `wsl-bash` (Ubuntu-18.04 via `Vampire/setup-wsl`); installs `gcc/g++-mingw-w64`, NSIS 3.11.0, OpenSSL 3.1.1; caches wxWidgets 3.2.6 keyed on the workflow file hash; runs `./otp_build env_win32 x64`, `configure`, `boot -a`, `release -a`, `installer_win32`. Artifact `otp_win32_installer` with `if-no-files-found: error`.
- `reusable-build-flavours.yaml` — inputs `base_branch`, `types` (whitespace list, `main.yaml` passes `"opt debug lcnt asan gcov valgrind"`). Matrix `flavor: [jit, emu]` × types; runs `otp_build boot -a`, `cerl -<type> -emu_flavor`, `make install TYPE/FLAVOR`, `otp_build release`, plus jit-only extras: `ERTS_JIT_ABI_WIN32=1`, LTTng trace build, and `+JPperf/+JMsingle/+JDdump` start-option checks.
- `reusable-build-otp.yaml` — input `base_branch`. Matrix `type: [32-bit, cross-compile, clang]`; the only step is `./.github/actions/build-base-image` (it validates that the other Dockerfiles build, it produces no artifact).
- `reusable-freebsd.yaml`, `reusable-openbsd.yaml`, `reusable-solaris.yaml` — input `base_branch`. All run on `ubuntu-latest` and build inside QEMU VMs via `vmactions/freebsd-vm`, `vmactions/openbsd-vm`, `vmactions/solaris-vm`; they download `otp_prebuilt`, install packages, and do plain `./configure && gmake`.
- `reusable-documentation.yaml` — inputs `base_branch`, `base_url` (default `''`), `trigger_erlang_org_build` (boolean); secret `TRIGGER_ERLANG_ORG_BUILD` (required).
  - Runs `./otp_build download_ex_doc`, `make release docs release_docs`, `make install-docs`, then `make release_docs DOC_TARGETS='html man'`; artifacts `otp_doc_html`, `otp_doc_man`; runs `scripts/otp_check_html_links.exs`; optionally dispatches `erlang/erlang-org` workflow `update-gh-cache.yaml`.
- `reusable-static-analysis.yaml` — input `base_branch`. `make format-check` (clang-format check), `scripts/run-dialyzer`, and `./.github/actions/ossf-compiler-flags-scanner` (SARIF upload enabled when repo is `erlang/otp` or the event is not a push — so fork PRs stay upload-free).
- `reusable-test.yaml` — inputs `base_branch`, `changes`; `env.BASE_BRANCH` at workflow level. Matrix type = `fromJson(inputs.changes)`, `fail-fast: false`. Special-cases `emulator` → `erts/emulator/`, `epmd` → `erts/epmd`, `debug` → `lib/os_mon` with `TYPE=debug`; runs `otp_build download_gdb_tools && make emulator && make TYPE=${TYPE} && make ${APP}_test TYPE=${TYPE}` in the `otp` container with `CTRUN_TIMEOUT=90` and surefire JUnit output. Artifact `<type>_test_results`.
- `reusable-system-test.yaml` — input `base_branch`. Downloads all `*_test_results` artifacts, merges them with `ct_run -refresh_logs`, runs `make system_test`, rewrites JUnit `file=` paths from container paths back to repo paths, and uploads `test_results` plus the `Unit Test Results` artifact (consumed by `pr-comment.yaml`).
- `reusable-sbom.yaml` — inputs `base_branch`, `full_build_and_check`, `otp_sbom_version`, `verify_sbom`; output `ort_version` (`ORT_VERSION = 92.2.0`).
  - Runs OSS Review Toolkit (analyzer/scanner/reporter/evaluator) from `ghcr.io/oss-review-toolkit/*`, overlays scan results with `reuse` via `.github/scripts/ort-scanner.es`, processes/verifies with `.github/scripts/otp-compliance.es sbom otp-info|test-file|vendor`.
  - Job `vendor-dependency-upload` submits the vendor SBOM to the GitHub Dependency Submission API only on `push` to `maint` (so Dependabot sees the shipped vendored libraries).
  - Artifacts: `ort-scan-results`, `spdx-sbom-result`, `ort-results-otp-<version>`.
- `reusable-release.yaml` — inputs `otp_sbom_version`, `vsn`, `tag`; secret `TRIGGER_ERLANG_ORG_BUILD` (required); permissions `contents/attestations/id-token/actions: write`.
  - Downloads `otp_prebuilt` + both doc archives, renames them with `.github/scripts/create-artifacts.sh artifacts "<tag>"`, writes `MD5.txt`/`SHA256.txt`, signs with `actions/attest-sbom` (`.sigstore` bundles), then `gh release create|edit` (with `--latest` computed by comparing the tag against the first line of `otp_versions.table` on `master`).
  - Two release code paths: `otp_sbom_version != ''` (OTP 28+, attestations + SBoM) and `== ''` (OTP 26/27, plain assets).
- `reusable-vendor-vulnerability-scanner.yml` — the only reusable workflow with a second trigger (`workflow_dispatch`); inputs `checkout` (bool, default false), `version` (string, default `master`), `fail_if_cve` (bool, default false); optional secrets `ERLANG_VENDOR_SCANNER_APP_ID`, `ERLANG_VENDOR_SCANNER_BOT_PRIVATE_KEY`.
  - `main.yaml` calls it with `fail_if_cve: false` and `secrets: inherit` — `fail_if_cve` must stay `false` there because the workflow opens a GitHub issue on a CVE and forked PRs must not be able to create issues.
  - Job `analysis-vendor-dependencies` always downloads `.github/scripts/otp-compliance.es` from `master` via the API (older maint branches lack the file) and runs `sbom osv-scan`; it uses a GitHub App token for non-forked runs, the default token otherwise.
- `reusable-vex-attest.yml` — `workflow_call` + `workflow_dispatch`, no inputs/secrets. Checks out the `openvex` branch, attests `otp-*.openvex.json` with `actions/attest`, then commits the `.sigstore` files back to the `openvex` branch with the Erlang bot app (`vars.ERLANG_BOT_APP_ID`, `secrets.ERLANG_BOT_PRIVATE_KEY`). No caller inside this repository.

### Standalone (event / schedule driven)
- `license-scanner.yaml` (`push`, `pull_request`) — `erlef/setup-beam` OTP 27, then `./scripts/license-header.es ci`, and re-runs `update` + `ci` to prove copyright updates are idempotent.
- `github-actions-checker.yaml` (`push`, `pull_request`) — `ghcr.io/sethvargo/ratchet check` over **all** `.github/**/*.yml|yaml` (workflows, actions and `zizmor.yml`); the header comment contains the fix recipe. This is the CI enforcement of SHA pinning.
- `pr-comment.yaml` (`workflow_run` on "Build and check Erlang/OTP", types `requested`/`completed`) — runs with a read-write token from the base repo (never checks out or executes fork code). Jobs: `pr-number` (uses `.github/scripts/get-pr-number.es` on OTP 27), `starting-tests` (posts the "starting" comment via `.github/scripts/pr-comment.js`), `finished-tests` (downloads `Unit Test Results` + `Event File` artifacts, publishes them with `EnricoMi/publish-unit-test-result-action`, then appends tips). Concurrency: workflow-wide `pr-comment`, and `erlang.github.io-deploy` for the finished job.
- `osv-scanner-scheduled.yml` (`pull_request`, `workflow_dispatch`, `schedule`) — only effective on `schedule` in `erlang/otp`: `schedule-scan` lists maintained branches via `.github/scripts/get-supported-branches.sh`, then `run-scheduled-scan` fans out `gh api .../reusable-vendor-vulnerability-scanner.yml/dispatches` with `ref=master` and `inputs[version]=<branch>`, `fail_if_cve=true`.
- `ossf-compiler-flags-scanner.yaml` (`workflow_dispatch`, `schedule`) — `erlang/otp` only; builds the base image and runs the OSSF compiler-flags scanner with SARIF upload.
- `scorecard.yaml` (`workflow_dispatch`, `branch_protection_rule`, `schedule` 13:00 UTC, `push` to `master`) — OSSF Scorecard with `publish_results: true`.
- `security-practice-scheduled.yml` (annual `schedule`, cron `0 9 25 10 *`) — opens a GitHub issue reminding maintainers to review `security.txt` and OpenChain licence/security compliance.
- `openvex-sync.yml` (`workflow_dispatch`, daily `schedule`) — runs on the `openvex` branch, copies `.github/scripts` and `otp_versions.table` from a temporary `master` worktree, then `.github/scripts/otp-compliance.es vex verify -p` to open PRs for new security advisories. Needs `vars.ERLANG_BOT_APP_ID` / `secrets.ERLANG_BOT_PRIVATE_KEY`.
- `sync-github-releases.yaml` (`workflow_dispatch`, hourly `schedule`) — logs in to `ghcr.io`, installs `.github/requirements/mdformat.txt` with `--require-hashes`, and runs `.github/scripts/sync-github-releases.sh <repo> "Bearer <token>" "^[2-9][1-9]\..*" 25m`.
- `update-base.yaml` (`workflow_dispatch`, daily 00:00 UTC) — matrix `type: [debian-base, ubuntu-base, i386-debian-base]` × `branch: [master, maint, maint-26, maint-27]`; builds with `.github/scripts/build-base-image.sh` and pushes `ghcr.io/<owner>/otp/<type>:latest` + `:<branch>`.
- `upload-windows-zip.yaml` (`workflow_dispatch` with `version` + `target` inputs) — on `windows-2025`; installs the released `otp_<target>_<version>.exe`, bundles a `.zip` with `vc_redist` and `INSTALL.txt`, smoke-tests `erl.exe +V`, and `gh release upload --clobber` to tag `OTP-<version>`.
- `sigstore-updater.yml` (`release: [edited]`) — re-attests an edited release's assets against `bom.spdx.json` and re-uploads the `.sigstore` files.
- `renovate-vendored-deps.yaml` (`pull_request` on `**/vendor.info`) — only for `renovate-bot` PRs whose title contains "update dependency"; checks out the PR head *with* credentials (needs them to push) and runs `.github/scripts/renovate-vendored-deps.sh`; on failure posts a truncated log as a PR comment.

## Constraints
- Every workflow file starts with the `%CopyrightBegin%` / `SPDX-License-Identifier: Apache-2.0` header block; `license-scanner.yaml` + `scripts/license-header.es` enforce it.
- Every third-party `uses:` is pinned to a full commit SHA with a trailing `# ratchet:<owner>/<repo>@<tag>` comment; never replace a pin with a moving tag (`github-actions-checker.yaml` fails the build).
- Repo-local composites are referenced unpinned as `./.github/actions/...` (not SHA checked by ratchet).
- Almost every `actions/checkout` uses `persist-credentials: false`; the exceptions (`renovate-vendored-deps.yaml`) carry an explicit `# zizmor: ignore[artipacked]` with a reason.
- Repository-scoped jobs gate on `github.repository == 'erlang/otp'` so forks can run CI without attempting releases, uploads, issue creation or bot authentication.
- Reusable workflows must be invoked from `main.yaml` with `needs:` wiring done by the caller — the callees cannot see `needs.pack` (see their `# NOTE: This job should only be executed on the callee with needs: pack` comments).
- Do not put untrusted PR metadata inline in `run:` scripts; pass it through `env:` (see `reusable-setup.yaml`).

## Design Decisions
- One central `main.yaml` plus `workflow_call` callees, rather than per-platform workflow files, so path-based change detection, caching and gating live in exactly one place.
- `reusable-pack.yaml` builds the 64-bit base **once** and publishes it as `otp_prebuilt` artifacts; every other build then re-uses that tar instead of re-running configure, because Docker images cannot be shared between jobs (see the comment block at the top of `main.yaml`).
- The 64-bit `pack` job builds its Docker image with `BUILD_IMAGE: false` (it only needs the base image), while every consumer uses `./.github/actions/build-base-image` with `BUILD_IMAGE: true` and pulls the prebuilt tars from the artifact cache.
- Change detection is expressed as generated `dorny/paths-filter` files: `.github/scripts/path-filters.sh` → `path-filters.yaml` (one entry per app) and `.github/scripts/c-code-path-filters.sh` → `c-code-path-filters.yaml`; `full_build_and_check` overrides the app list with all apps.
- `ruleset-checker` exists so GitHub rulesets can require a stable set of job names even though those jobs are conditional (skips are treated as success).
- `pr-comment.yaml` uses `workflow_run` instead of `pull_request_target` so fork PRs can receive comments with a write token while never executing fork code.
- Secrets are passed explicitly by name (`TRIGGER_ERLANG_ORG_BUILD`, vendor scanner app secrets) except for the internal vendor scanner, which uses `secrets: inherit` with a documented `zizmor: ignore[secrets-inherit]`.
- `zizmor` policy lives in `.github/zizmor.yml`, which disables `misfeature` (Windows CMD shells are intentional) and `secrets-outside-env` (cross-repo triggers and bot auth are not yet in environments).

## Notes for Agents
- Runners are **all GitHub-hosted** — there are no self-hosted runners in this repo: `ubuntu-latest` for everything Linux (including FreeBSD/OpenBSD/Solaris, which run inside QEMU via `vmactions/*-vm`), `windows-2025` for Windows, `macos-15` (arm64) / `macos-15-intel` (x86-64) for the Apple targets.
- Cleanup is delegated: `.github/actions/build-base-image` runs `.github/scripts/cleanup_gh_runner.sh` on every Linux build/test job to free disk; `update-base.yaml` calls the same script directly.
- The workflow/cache plumbing uses a locally built docker image tagged simply `otp`; base images come from `ghcr.io/<owner>/otp/<type>` and the OSSF/SBOM tooling from `ghcr.io/oss-review-toolkit/*`.
- File extension naming is inconsistent: most files are `.yaml`, but `openvex-sync`, `osv-scanner-scheduled`, `renovate-vendored-deps`, `security-practice-scheduled`, `sigstore-updater`, `reusable-vendor-vulnerability-scanner` and `reusable-vex-attest` are `.yml`; ratchet and Renovate globs cover both.
- `reusable-test.yaml` builds the `make TYPE=${TYPE} ...` line with a shell variable `TYPE` that is only assigned in the `debug` case (so it is empty for all other apps, meaning "default type") — careful not to "fix" it by exporting a global `TYPE`.
- In `reusable-pack.yaml` the cache-restore step derives `NO_CACHE`/`DELETED` from `dorny/paths-filter` counts (`deleted_count > 20`); the thresholds and the `full_build_and_check` fallback are intentional.
- `reusable-build-windows.yaml` computes `cache-hit` manually because `actions/cache` on Windows sometimes does not set it.

## Known Issues
- The `solaris` job in `main.yaml` is permanently skipped via `if: ... && false` (comment: the VM image is broken); `reusable-solaris.yaml` is kept buildable but unreachable.
- `osv-scanner-scheduled.yml` triggers on `pull_request` and its header comment claims PR/push scanning, but every job is gated on `github.event_name == 'schedule'`, so pull-request runs are complete no-ops.
- `reusable-vex-attest.yml` declares `workflow_call` but nothing in this repository calls it; it is reachable only through manual `workflow_dispatch` (or from another repository).
- `reusable-vex-attest.yml` and `reusable-vendor-vulnerability-scanner.yml` read repo-scoped configuration (`vars.ERLANG_BOT_APP_ID`, `secrets.ERLANG_*`) that does not exist in forks, so their jobs cannot be fully exercised outside `erlang/otp`.
- `reusable-setup.yaml` outputs `is_tag_push` and `repository`, and `reusable-pack.yaml` outputs `all`, but none of them is exposed at workflow level or consumed by any caller.

## Dependencies
- External Actions pinned by SHA: `actions/{checkout,upload-artifact,download-artifact,cache,attest,attest-sbom,create-github-app-token,github-script}`, `erlef/setup-beam`, `dorny/paths-filter`, `docker/login-action`, `EnricoMi/publish-unit-test-result-action`, `Vampire/setup-wsl`, `vmactions/{freebsd,openbsd,solaris}-vm`, `oss-review-toolkit/ort-ci-github-action`, `advanced-security/spdx-dependency-submission-action`, `ossf/scorecard-action`, `zizmorcore/zizmor-action`, `github/codeql-action/upload-sarif`, `openvex/setup-vexctl`, `docker://ghcr.io/sethvargo/ratchet`.
- Registries/images: `ghcr.io/<owner>/otp/{debian-base,ubuntu-base,i386-debian-base}`, `ghcr.io/oss-review-toolkit/ort[-minimal]`.
- Repo variables/secrets referenced: `TRIGGER_ERLANG_ORG_BUILD`, `ERLANG_BOT_APP_ID` (var), `ERLANG_BOT_PRIVATE_KEY`, `ERLANG_VENDOR_SCANNER_APP_ID`, `ERLANG_VENDOR_SCANNER_BOT_PRIVATE_KEY`.
- Python requirements for `sync-github-releases.yaml` come from `.github/requirements/mdformat.txt` (hashed).
- Renovate keeps action pins fresh from the repo-root `renovate.json5` (github-actions manager scoped to `.github/workflows/*.y{a,}ml` and `.github/actions/*/*.y{a,}ml`, with upload/download-artifact grouped in lockstep) and from `**/vendor.info` files.

## See Also
- `.github/CONTEXT.md` — parent node; overview of CI/CD assets.
- `.github/actions/` — composites `build-base-image` (the shared Linux base-image/cache entry point used by nearly every job) and `ossf-compiler-flags-scanner`.
- `.github/scripts/` — all helper scripts and escripts invoked by these workflows (`path-filters.sh`, `build-base-image.sh`, `init-pre-release.sh`, `restore-from-prebuilt.sh`, `cleanup_gh_runner.sh`, `otp-compliance.es`, `pr-comment.js`, `sync-github-releases.sh`, …).
- `.github/dockerfiles/` — `Dockerfile.64-bit`, `Dockerfile.32-bit`, `Dockerfile.clang`, `Dockerfile.cross-compile` and the debian/ubuntu base images.
- `./scripts/` (repo root) — `build-otp-tar`, `run-dialyzer`, `license-header.es`, `otp_check_html_links.exs` are invoked directly from workflows.
- `./otp_build` and `./make/` — configure/boot/release/install driver and make targets (`emulator`, `<app>_test`, `system_test`, `release_docs`, `format-check`, `install-docs`) that the workflows shell out to.
- `./renovate.json5` — config governing action-SHA and vendored-dependency update PRs.
- `./HOWTO/` and `./system/` — build/test documentation referenced by the CI steps.
- Read-only siblings: `.github/ISSUE_TEMPLATE/`, `.github/requirements/` — read-only from this node; escalate changes to `.github/`.

## Routing Table
- `main.yaml` → orchestrator: triggers, change detection, gating, ruleset checker, zizmor job.
- Reusable — build base & platforms:
  - `reusable-setup.yaml` → derived ref/branch/version outputs (no build).
  - `reusable-pack.yaml` → 64-bit base build, change detection, `otp_prebuilt`/`otp_git_archive` artifacts.
  - `reusable-build-otp.yaml` → 32-bit / cross-compile / clang base-image builds.
  - `reusable-build-flavours.yaml` → emu/jit flavors, types (opt/debug/lcnt/asan/gcov/valgrind), LTTng, JIT-win32 ABI.
  - `reusable-build-macos.yaml`, `reusable-build-ios.yaml`, `reusable-build-windows.yaml` → Apple and Windows packages.
  - `reusable-freebsd.yaml`, `reusable-openbsd.yaml`, `reusable-solaris.yaml` → BSD/Solaris VM builds.
- Reusable — test & verify:
  - `reusable-check-files.yaml` → PR whitespace/BEAM-file gate.
  - `reusable-test.yaml`, `reusable-system-test.yaml` → per-app `make <app>_test` and `make system_test`.
  - `reusable-static-analysis.yaml` → clang-format, Dialyzer, OSSF compiler flags/SARIF.
  - `reusable-documentation.yaml` → docs build, html/man archives, link check, erlang.org trigger.
- Reusable — compliance & release:
  - `reusable-sbom.yaml` → ORT SBOM generation/verification and dependency submission.
  - `reusable-vendor-vulnerability-scanner.yml` → OpenVEX/OSV scan of vendored deps (also `workflow_dispatch`).
  - `reusable-release.yaml`, `reusable-vex-attest.yml` → GitHub release creation/attestation, OpenVEX sigstore attestations.
- Security scanning (standalone): `license-scanner.yaml`, `github-actions-checker.yaml`, `osv-scanner-scheduled.yml`, `ossf-compiler-flags-scanner.yaml`, `scorecard.yaml`, `security-practice-scheduled.yml`.
- Release & publishing (standalone): `sync-github-releases.yaml`, `upload-windows-zip.yaml`, `sigstore-updater.yml`, `openvex-sync.yml`.
- PR feedback (standalone): `pr-comment.yaml`.
- Maintenance (standalone): `update-base.yaml` (base docker images), `renovate-vendored-deps.yaml` (vendored dependency refresh).
