# GitHub Actions & CI/CD Assets

## Intent
Everything GitHub-facing for the Erlang/OTP repository: the CI/CD pipeline (build, test, documentation, release), supply-chain security scanning (licenses, SBOM, OpenVEX, OSV, Scorecard, OpenSSF compiler flags), release publishing automation, and the issue templates.
The pipeline is the *only* automated gate on the `erlang/otp` repository; there is no in-tree CI runner configuration besides `Makefile`/`otp_build` targets invoked from these workflows.
This directory is also the Docker **build context** for all CI images (see `dockerfiles/`).

## API Surface
- `workflows/main.yaml` — the single orchestrator ("Build and check Erlang/OTP"), triggered by `push`, `pull_request` and a nightly `schedule` (cron `0 1 * * *`). Computes the changed applications and build flags once, builds one 64-bit base, then fans out to ~18 `workflow_call` targets (per-platform builds, tests, docs, static analysis, SBOM, release).
- `workflows/reusable-*.y*ml` — 20 reusable workflows, consumed via `workflow_call` from `main.yaml` (also callable by the release tooling). They are not standalone entry points; their inputs/outputs/secrets are the CI contract.
- `workflows/*.y*ml` (standalone) — independently triggered security, publishing and maintenance jobs (`license-scanner`, `github-actions-checker`, `osv-scanner-scheduled`, `ossf-compiler-flags-scanner`, `scorecard`, `security-practice-scheduled`, `openvex-sync`, `sync-github-releases`, `upload-windows-zip`, `sigstore-updater`, `pr-comment`, `update-base`, `renovate-vendored-deps`).
- `actions/build-base-image/` — composite action that builds/caches the shared Linux docker image tagged `otp`; every Linux build/test job runs inside it.
- `actions/ossf-compiler-flags-scanner/` — composite action wrapping the OpenSSF compiler-flag scan and SARIF generation.
- `scripts/` — helper scripts and escripts invoked by the workflows/actions (changed-path filters, base-image build, prebuilt-cache restore, release sync, `otp-compliance.es`, `pr-comment.js`, …).
- `dockerfiles/Dockerfile.<TYPE>` — `TYPE` ∈ {`64-bit`, `32-bit`, `clang`, `cross-compile`} plus the `ubuntu-base`/`debian-base` images published to `ghcr.io/erlang/otp/*`; `TYPE` string is the CI selector for which Dockerfile to use.
- `requirements/mdformat.txt` (`mdformat-in.txt` is the input) — hashed pip requirements used by `sync-github-releases.yaml`.
- `zizmor.yml` — policy file for the `zizmor` GitHub Actions security linter (run as a job in `main.yaml`).
- `ISSUE_TEMPLATE/bug_report.md`, `ISSUE_TEMPLATE/feature_request.md` — GitHub issue forms.

### How CI reaches the rest of the repo
Workflows shell out to repo-root helpers: `scripts/build-otp-tar`, `scripts/run-dialyzer`, `scripts/license-header.es`, `scripts/otp_check_html_links.exs`; the `otp_build` driver (`configure`, `boot -a`, `release -a`, `env_win32`, `installer_win32`, `download_gdb_tools`, `download_ex_doc`); and make targets (`emulator`, `<app>_test`, `system_test`, `format-check`, `release docs release_docs`, `install-docs`, `install`).
Changed-path detection is generated at run time: `scripts/path-filters.sh` and `scripts/c-code-path-filters.sh` emit the `dorny/paths-filter` config consumed by `reusable-pack.yaml`.

## Constraints
- Every `uses:` of a third-party action must be pinned to a full commit SHA with a trailing `# ratchet:<owner>/<repo>@<version>` comment. Enforced in CI by `workflows/github-actions-checker.yaml` (`ratchet check` over all `.github/**/*.y{a,}ml`) and refreshed by Renovate (root `renovate.json5`) / `scripts/update-gh-actions-versions.es`. Never hand-edit a pin to a tag.
- Every file here is covered by the SPDX license-header scanner (`workflows/license-scanner.yaml` → `scripts/license-header.es ci`); GitHub Actions YAML uses `##`-prefixed `%CopyrightBegin%`/`%CopyrightEnd%` blocks (`.yml` variants use `#`). `.github/**` is not exempt.
- Jobs that need write tokens or repo-scoped secrets must keep the `github.repository == 'erlang/otp'` guard so that fork builds stay read-only; PR-triggered workflows must never execute fork code with a write token (`pr-comment.yaml` deliberately uses `workflow_run` instead of `pull_request_target`).
- Suppressing a zizmor finding requires an inline `# zizmor: ignore[<rule>]` with a justification, or an entry in `zizmor.yml`.
- The Docker build context is `.github/`, not the repo root — `COPY` paths in `dockerfiles/` are relative to `.github/` and no repository source may be baked in besides the prebuilt `otp.tar.gz`.
- Generated/untracked leftovers of a CI run (`scripts/path-filters.yaml`, `scripts/c-code-path-filters.yaml`, `otp*.tar.gz`) must never be committed.

## Design Decisions
- **One orchestrator + reusable workflows**: change detection, caching and the 64-bit build happen exactly once in `main.yaml` → `reusable-setup.yaml` / `reusable-pack.yaml`; everything else is a `workflow_call` job gated on `pack` outputs (`build-c-code`, `changes`). This avoids rebuilding OTP per platform and keeps triggers/permissions in one reviewable place. Note that reusable callees cannot see `needs` from the caller, so `reusable-pack.yaml` re-exports `base_branch` as a workflow output.
- **Docker base-image approach**: Linux jobs do not build OTP from scratch; they consume the `otp` image produced from a prebuilt tar (`otp_prebuilt` artifact / `actions/cache`), with a daily cache invalidation so `apt-get upgrade`/`kerl` drift cannot produce a stale environment.
- **`ruleset-checker`**: GitHub rulesets fail on skipped required jobs, so a final `if: always()` job inspects `toJSON(needs)` and treats *skipped* as success while failing on `failure`/`cancelled`.
- **Supply chain**: release artifacts are attested (sigstore/attestations), an SBOM is produced and verified, and vendored-dependency findings are mapped to OpenVEX statements; all of this is gated to the upstream repository.
- **All runners are GitHub-hosted** (`ubuntu-latest`, `windows-2025`, `macos-15`/`macos-15-intel`); BSD/Solaris jobs run inside QEMU VMs (`vmactions/*-vm`) on `ubuntu-latest`, and no self-hosted runner infrastructure exists.

## Known Issues
- The Solaris job in `main.yaml` is permanently disabled (`if: ... && false`, broken VM image), so `reusable-solaris.yaml` is currently unreachable.
- `osv-scanner-scheduled.yml` also triggers on `pull_request`, but every job is gated to `schedule`, so PR runs are no-ops.
- `reusable-vex-attest.yml` has no in-repo caller (dispatch-only) and, like `reusable-vendor-vulnerability-scanner.yml`, depends on repo-scoped variables/secrets that do not exist in forks.
- Workflow file extensions are inconsistent (mostly `.yaml`, seven files use `.yml`); ratchet/Renovate globs cover both.

## Notes for Agents
- Read only the file you need: workflows are self-describing YAML with license headers; the per-directory CONTEXT.md files below carry the contracts, so avoid re-deriving them.
- Debugging CI changes locally is limited — most steps assume a GitHub Actions environment (`$GITHUB_OUTPUT`, artifacts, ghcr.io login). `.github/scripts/path-filters.sh`, `c-code-path-filters.sh`, `get-*-versions.sh` and the escripts can be run locally.
- When adding a new platform or job, wire it in `main.yaml` and add it to `ruleset-checker.needs`, otherwise it is invisible to branch protection.
- CI is the authoritative integration test for OTP (`make test`/`make system_test`), separate from per-application test suites; use `HOWTO/TESTING.md` for local equivalents.

## Dependencies
- GitHub-hosted runners (ubuntu-latest, windows-2025, macos-15/macos-15-intel); `ghcr.io/erlang/otp/*` registry for base images.
- Repo secrets/vars: `GITHUB_TOKEN`, `TRIGGER_ERLANG_ORG_BUILD`, `ERLANG_BOT_APP_ID`/`ERLANG_BOT_PRIVATE_KEY`, `ERLANG_VENDOR_SCANNER_APP_ID`/`ERLANG_VENDOR_SCANNER_BOT_PRIVATE_KEY`.
- External tooling on runners or inside the `otp` image: docker/BuildKit, `jq`, `curl`, `rsync`, `python3` + hashed pip requirements, `gh`, `escript` (OTP 27+, some scripts 28+), `vexctl`, `ratchet`, `zizmor`.
- Third-party actions pinned by SHA (`actions/*`, `erlef/setup-beam`, `dorny/paths-filter`, `vmactions/*-vm`, `oss-review-toolkit/ort-ci-github-action`, `ossf/scorecard-action`, `zizmorcore/zizmor-action`, …).

## See Also
- `../CONTEXT.md` — repository root: build system (`configure`, `otp_build`, `Makefile`), `OTP_VERSION`.
- `../scripts/` — repo-root helpers invoked directly by workflows (`build-otp-tar`, `run-dialyzer`, `license-header.es`).
- `../make/` and `../otp_build` — the build targets CI drives.
- `../renovate.json5` — dependency/action-pin update configuration.
- `../HOWTO/` — INSTALL/TESTING/DEVELOPMENT docs that mirror what CI enforces.

## Routing Table
- `workflows/` → all GitHub Actions workflows: the `main.yaml` orchestrator, the reusable `workflow_call` contract, per-platform builds, tests, docs, SBOM/release and standalone security/maintenance workflows.
- `scripts/` → CI helper scripts and escripts (path filters, base-image build, prebuilt-cache restore, release/OpenVEX/SBOM automation, GitHub metadata helpers).
- `actions/` → composite actions `build-base-image` (shared Linux image + cache) and `ossf-compiler-flags-scanner`.
- `dockerfiles/` → base and derived CI images (`ubuntu-base`, `debian-base`, `64-bit`, `32-bit`, `clang`, `cross-compile`) plus their ODBC/log4j/profile config files.
- `requirements/` → hashed pip requirement set for `mdformat` used by the release-sync workflow (`mdformat.txt` is generated from `mdformat-in.txt` with pip-compile; do not edit the generated file).
- `ISSUE_TEMPLATE/` → GitHub bug-report and feature-request templates (maintained by the OTP team, not CI).
- `zizmor.yml` → zizmor linter policy for the Actions workflows.
