# Workflow
- Changes are planned by the planner agent.
- The plan is implemented by the implementer agent.
- Reviews are done by the reviewer agent.
- Until the initial setup is complete, only planner and implementer are used. Do NOT invoke the reviewer automatically; the user runs it manually once the setup is done.

# Project: betbot
Rust tipping bot (Tippspiel). It runs as a Kubernetes **CronJob** twice a week and exits afterwards.
Use tokio where needed and follow Rust best practices (clippy clean, rustfmt, no unwrap in non-test code, typed errors).

## Toolchain
- `flake.nix` with flake-parts and nixpkgs-unstable. Dev shell: `nix develop`, or direnv via `.envrc`.
- Rust is pinned in `rust-toolchain.toml` through oxalica/rust-overlay. CI uses the same file.
- TLS via reqwest with rustls, no openssl.

## Delivery
- Deployment via Helm chart.
- GitHub Actions build the Docker image (GHCR) and the Helm chart.
- The Helm chart is served via GitHub Pages.
- Semantic Versioning.

## Architecture
Three swappable components, trait-based, with interchangeable implementations:
1. **Tip platform**: user login and submitting tips.
2. **Odds provider**: odds come from the same tip platform or from another one.
3. **Algorithm/strategy**: takes the odds and computes the tip result.

First target: Bundesliga, first platform kicktipp.de.

## Rules on tipped results
Special rules are configurable. Example: the same result (2:1 and 1:2 count as the same) must not be tipped more than 6 times. The scope (season or matchday) is still to be defined.

## Multi-tenant configuration
- A ConfigMap defines per tenant: user, platform, algorithm, rules.
- Credentials live in Kubernetes Secrets. The ConfigMap only references a user from a Secret.
- Default proposal: non-secret platform data (e.g. kicktipp community name) in the ConfigMap, Secret holds only credentials (login/password).

# Status / next steps
- Done: toolchain bootstrap (flake, rust-toolchain 1.100.0, skeleton).
- Decided (2026-11-15): algorithm = **Poisson model + expected-points maximization**.
  Remove bookmaker margin from 1X2 odds → implied probabilities → fit expected goals (λ home/away) → score-probability matrix → pick the tip with the highest *expected platform points* (scoring scheme configurable, kicktipp default 4/3/2), not simply the most likely score.
  Tip rules (e.g. max 6× the same result) are applied as constraints: when a limit is hit, switch the match with the smallest expected-points loss to its next-best tip.
  Rejected for now: fixed odds→score mapping (suboptimal); Dixon-Coles low-score correction may be added later as an alternative algorithm implementation (the algorithm is swappable).
- Decided (2026-11-15):
  - kicktipp shows bookmaker odds on the tip submission page → odds provider `platform` reads them from there.
  - Tip window: only the complete NEXT matchday is tipped, never more than one matchday ahead. Implemented as a runner filter (independent of the platform): next matchday = lowest matchday with at least half as many open matches as the largest open matchday; postponed stragglers of earlier matchdays are tipped too, later matchdays never.
  - Unsatisfiable tip rules → log a warning and keep the best tip (no failure) for now.
  - User's community scoring: exact result 3, correct goal difference 2 (tip 1:0, result 2:1), correct tendency 1 (tip 2:1, result 3:1). Draws also get goal-difference points (tip 1:1, result 2:2 → 2), i.e. 3/2/1 for all outcomes. Scoring stays configurable per tenant; kicktipp default (4/3/2, no difference points for draws) remains the code default.
  - The user's kicktipp round shows "Strategieverbot Ergebnis aktiv ( max. 6/9 Tipps mit gleichem Ergebnis )" → the same-result rule is **per matchday** (max 6 of 9), mirrored results (2:1/1:2) count as the same.
  - kicktipp HTML fixtures (anonymised) live in `tests/fixtures/kicktipp/`. Fixtures must reflect the **server-rendered** HTML (view-source), not the browser DOM: kicktipp's JS adds classes like `tr.datarow`, `td.cell.colN`, `table.ktable` that the bot never sees (this broke the first real dry run).
  - kicktipp navigation: after login go to `/{community}/tippabgabe`; without URL parameters it always shows the CURRENT matchday (may be running, i.e. rows already closed). Specific matchday: `?tippsaisonId=<id>&spieltagIndex=<n>`; `tippsaisonId` is read from the page (hidden input), never configured. If the current page has no open matches, the bot moves exactly one matchday ahead.
- Done: core skeleton, kicktipp client (real dry run against the user's round succeeded 2026-11-16).
- Done: Dockerfile (rust:1.100.0-trixie builder → gcr.io/distroless/cc-debian13:nonroot, uid 65532; rust-toolchain.toml deliberately not copied, so the builder tag must match its channel), Helm chart `charts/betbot` (CronJob, podFailurePolicy FailJob on exit 2, per-tenant `existingSecret` mounted at /secrets/<tenant>, ConfigMap from `values.config` with `existingSecret` stripped, optional NetworkPolicy), CLI flag `--check-config`.
- Decided (2026-11-16):
  - GitHub owner: `mrsteakhouse` → image `ghcr.io/mrsteakhouse/betbot`, chart repo `https://mrsteakhouse.github.io/betbot`.
  - Image architectures: linux/amd64 + linux/arm64.
  - Default CronJob schedule: Monday + Friday 10:00 Europe/Berlin (`0 10 * * 1,5`).
  - Chart version == app version (both bumped by release-please); `appVersion` is the default image tag. Credentials only via user-created Secrets (keys `username`/`password`), never in values.
- Decided (2026-11-16): license `MIT OR Apache-2.0` (LICENSE-MIT, LICENSE-APACHE).
- Done: first release v0.3.0 (image + chart published 2026-11-16).
- Decided (2026-11-16): tip history is read per `HistoryScope`: `TipPlatform::placed_tips(scope)` with `Season` (all matchdays) or `Matchdays(window)` (platforms may return more, never less). `Rules::history_scope(window)`: no rule → no history, any `scope: season` rule → Season, otherwise Matchdays. For matchday-only rules kicktipp needs no extra requests: the cached tip page holds all tips of the matchday incl. started rows.
- Decided (2026-11-16): notifications are in-house (`src/notify`, no notification crate) with shoutrrr-style URLs. Services: `telegram`, `discord`, `ntfy`, `generic` (webhook); SMTP deferred. URLs carry tokens → only from Secret files (`notifiers[].urlFile`, chart `existingSecret` with key `url` mounted at `/notifiers/<name>/url`); an inline `url` key is rejected. Tenants reference notifiers by name (`notifiers: [..]`; unknown/duplicate reference = invalid config). Exactly ONE German summary per tenant and run (matchday incl. stragglers, one line per submitted tip, status ok/warnings/error), sent only if ≥1 tip was submitted, a warning occurred or the tenant failed — nothing to do → nothing sent. Dry run never sends, only logs `dry run: would notify` with the full text. Broken notifier → warning + disabled, failed send → warning; never changes the exit code. `--check-config` exits 2 only if an existing url file is malformed.
  `--test-notifiers`: sends one German test message to every configured notifier (not only referenced ones), then exits; no platform/credentials/tenants. Exclusive with `--check-config`/`--dry-run`. Unlike normal runs failures are visible: exit 2 if no notifier is configured or a URL file is missing/unreadable/invalid, 1 if a send fails or times out (15 s), 0 otherwise. Logs only notifier name and kind, never the URL.

# Delivery / CI
- CI: `.github/workflows/ci.yml` (fmt, clippy, nextest, cargo-deny, `scripts/check-versions.sh`, helm lint + ct lint, rendered chart config → `betbot --check-config`, docker build). Weekly RUSTSEC check: `audit.yml`.
- Releases: release-please (Conventional Commits) → tag `vX.Y.Z` → native amd64 + arm64 builds merged into one multi-arch image `ghcr.io/mrsteakhouse/betbot` (+ provenance attestation) → chart via chart-releaser to GitHub Pages (`https://mrsteakhouse.github.io/betbot`). All in `.github/workflows/release.yml`, strictly in that order.
- Versions in `Cargo.toml`, `Cargo.lock` and `charts/betbot/Chart.yaml` (`version` + `appVersion`) are owned by release-please — never edit them by hand. Chart version == app version.
- Never click "Update branch" on the release-please PR and never push to its branch: release-please regenerates it on every push to main. If it gets into a broken state, close it and delete its branch; the next push to main creates a fresh one.
- The image name `ghcr.io/mrsteakhouse/betbot` belongs to this repo. The old Python implementation (`mrsteakhouse/kicktipp-betbot`, still running in the cluster) publishes as `ghcr.io/mrsteakhouse/betbot-legacy`.
- `gh-pages` holds the Helm repo index (managed by chart-releaser); GitHub Pages serves it from that branch.
- Commit messages MUST follow Conventional Commits (`feat:`, `fix:`, `chore:`, `ci:`, `docs:` …; `feat!:` / `BREAKING CHANGE:` for breaking changes). Pre-1.0: `feat` bumps minor.
- PRs are squash-merged and the PR title becomes the commit message release-please parses, so PR titles must be Conventional Commits too (types feat, fix, perf, revert, docs, style, chore, refactor, test, build, ci). Enforced by `.github/workflows/pr-title.yml` (job `pr-title`, add it as required status check together with `consistency`, `rust`, `chart`, `docker`). Release-please PRs opened with `GITHUB_TOKEN` trigger neither CI nor this check → use `RELEASE_PLEASE_TOKEN` (fine-grained PAT) or admin bypass.
- Rust toolchain: `rust-toolchain.toml`; the Dockerfile builder tag (`rust:<channel>-trixie`) must match (checked by `scripts/check-versions.sh`; Dependabot ignores the `rust` image for that reason).
- GitHub Actions are pinned to full commit SHAs with a `# vX.Y.Z` comment; Dependabot updates actions, cargo and docker base images.
- Git author for this repo: `mrsteakhouse1@gmail.com` (set in local git config).
- `main` is protected by a ruleset: no direct pushes, every change goes through a PR from a feature branch (squash merge, PR title = Conventional Commit). The `gh` CLI is part of the dev shell (`nix develop -c gh pr create ...`).
