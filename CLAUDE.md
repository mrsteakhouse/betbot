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
- Open: license.
- Next: trait design, config and Secret schema, kicktipp client, Helm chart, GitHub Actions.
