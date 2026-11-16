# betbot

A tipping bot (Tippspiel) for football prediction games. It logs in to the tip
platform, reads the open matches and the bookmaker odds, computes the tips with
the highest expected points and submits them. It runs as a Kubernetes CronJob
twice a week and exits afterwards.

The first supported platform is [kicktipp.de](https://www.kicktipp.de) (Bundesliga).

- **Multi-tenant:** one run handles any number of tipping accounts (tenants),
  each with its own platform, algorithm, scoring scheme and rules.
- **Swappable components:** tip platform, odds provider and algorithm are
  separate traits with interchangeable implementations.
- **Safe to repeat:** existing tips are never overwritten (unless configured),
  so a retried or extra run changes nothing.

## How it works

1. **Tip window:** only the next complete matchday is tipped, never more than
   one matchday ahead. Postponed matches of earlier matchdays are tipped too.
2. **Odds:** kicktipp shows bookmaker odds (1X2) on the tip page; the
   `platform` odds provider reads them from there.
3. **Algorithm `poisson`:**
   1. Remove the bookmaker margin from the 1X2 odds (proportional) to get
      outcome probabilities.
   2. Fit the expected goals of both teams (Poisson λ home/away) to these
      probabilities.
   3. Build the probability matrix of all exact results.
   4. Pick the tip with the highest *expected points* under the tenant's
      scoring scheme. That is not necessarily the most likely result.
   5. Optional `variation`: instead of always the best tip, pick one of the
      tips within `tolerance` expected points of the best, deterministically
      from a per-tenant seed and the team names. Tenants in the same round
      then do not all tip the same.
4. **Rules:** constraints such as "the same result at most 6 times per
   matchday" are applied afterwards. When a limit is hit, the match that loses
   the fewest expected points switches to its next-best tip. Unsatisfiable
   rules log a warning and keep the best tips.
5. **Submit:** the tips are submitted and verified by reloading the page.
   With `--dry-run` they are only logged.

## Configuration

betbot reads one YAML file (`--config`, in Kubernetes a ConfigMap). Credentials
are never part of it: each tenant points to a directory with the files
`username` and `password` (a mounted Secret).

```yaml
tenantTimeoutSeconds: 300          # per-tenant time limit (default 300)
tenants:
  - name: alice                    # unique, used in logs
    credentialsDir: /secrets/alice # files `username` and `password`
    platform:
      type: kicktipp
      community: my-round-slug     # from https://www.kicktipp.de/<slug>/
    odds: { type: platform }       # default
    algorithm:
      type: poisson                # default
      maxTipGoals: 5               # default, 1..=10
      variation: { seed: 42, tolerance: 0.05 }  # optional, see below
    scoring:                       # default: kicktipp standard 4/3/2
      homeWin: { tendency: 1, goalDifference: 2, exact: 3 }
      draw:    { tendency: 1, goalDifference: 2, exact: 3 }
      awayWin: { tendency: 1, goalDifference: 2, exact: 3 }
    rules:
      maxSameResult: { max: 6, scope: matchday, mirrored: true }
    overwriteExistingTips: false   # default
```

| Key | Default | Meaning |
| --- | --- | --- |
| `tenantTimeoutSeconds` | `300` | Time limit per tenant; a tenant that exceeds it fails, the others continue. |
| `tenants[].name` | required | Unique tenant name (no surrounding whitespace). |
| `tenants[].credentialsDir` | required for kicktipp | Directory with the files `username` and `password`. One trailing newline is tolerated. |
| `tenants[].platform.type` | required | `kicktipp` or `fake` (offline fixture for local tests). |
| `tenants[].platform.community` | required for kicktipp | Slug of the tipping round, `[A-Za-z0-9_-]`. |
| `tenants[].platform.fixture` | required for fake | Path to a fixture file, see `examples/fake-data.yaml`. |
| `tenants[].odds.type` | `platform` | Where the odds come from. |
| `tenants[].algorithm.type` | `poisson` | Tip algorithm. |
| `tenants[].algorithm.maxTipGoals` | `5` | Highest number of goals per team that is tipped (1–10). |
| `tenants[].algorithm.variation.seed` | – (block optional) | Integer seed of the tip variation; required when `variation` is set. |
| `tenants[].algorithm.variation.tolerance` | – (block optional) | Maximum expected-points loss against the best tip (0–1, `0` = no variation); required when `variation` is set. |
| `tenants[].scoring` | kicktipp 4/3/2 | Points per outcome (`homeWin`, `draw`, `awayWin`) for `tendency`, `goalDifference` (optional) and `exact`. The default gives no goal-difference points for draws. |
| `tenants[].rules.maxSameResult.max` | – | Maximum number of tips with the same result (≥ 1). |
| `tenants[].rules.maxSameResult.scope` | – | `matchday` or `season`. |
| `tenants[].rules.maxSameResult.mirrored` | `true` | Count 2:1 and 1:2 as the same result. |
| `tenants[].overwriteExistingTips` | `false` | Replace tips that are already placed (e.g. manual ones). |
| `tenants[].notifiers` | `[]` | Names of notifiers that receive this tenant's run summary (see [Notifications](#notifications)). |
| `notifiers[].name` | – | Unique notifier name. |
| `notifiers[].urlFile` | – | File with the notifier URL (a mounted Secret). |

Match the scoring scheme and rules to your round's settings. kicktipp shows a
result limit as "Strategieverbot Ergebnis (max. 6/9 Tipps mit gleichem
Ergebnis)" on the tip page, which corresponds to
`maxSameResult: { max: 6, scope: matchday, mirrored: true }`.

`algorithm.variation` picks uniformly among all tips whose expected points are
at most `tolerance` below the best one, using the seed and the two team names.
For identical odds the pick is always the same, but odds drift between runs
can change it, which matters with `overwriteExistingTips: true`. Different
seeds only lead to different tips where several tips are close in expected
points; for clear favourites everybody still gets the best tip. Rules are
applied to the varied pick; if it hits a limit, the bot may fall back to the
pure best tip.

A complete example with an offline tenant is in
[`examples/config.yaml`](examples/config.yaml).

## Notifications

betbot can send a short summary per tenant and run to Telegram, Discord,
[ntfy](https://ntfy.sh) or any HTTP webhook. Notifiers are defined once and
referenced by name from tenants:

```yaml
notifiers:
  - name: telegram-main
    urlFile: /notifiers/telegram-main/url   # mounted Secret, holds the URL
tenants:
  - name: alice
    # ...
    notifiers: [telegram-main]
```

The notifier URL (shoutrrr style) contains tokens, so it is only read from
`urlFile` and never written into the config (an inline `url` key is rejected):

| Service | URL |
| --- | --- |
| Telegram | `telegram://<bot id>:<secret>@telegram?chats=@channel,123456[&notification=no]` |
| Discord | `discord://<webhook token>@<webhook id>` (from `https://discord.com/api/webhooks/<id>/<token>`) |
| ntfy | `ntfy://[[user]:password@]host[:port]/topic[?scheme=http&priority=1..5&tags=a,b]` (`:token@` = access token) |
| Webhook | `generic://[user:password@]host[:port]/path[?disabletls=yes&template=json\|plain&titlekey=title&messagekey=message]` |

Values in the URL must be percent-encoded where needed (e.g. `@` in a
password as `%40`). The generic webhook POSTs `{"title", "message", "level"}`
as JSON (or title and text as `text/plain` with `template=plain`) to
`https://host/path`; other query parameters are passed on to the target.

**What is sent:** exactly one message per tenant and run, in German: the
matchday (plus postponed matches of earlier matchdays), one line per submitted
tip (`FC Nordstadt - SV Südhafen  2:1`) and the status: all fine, the warnings
(matches without odds, unsatisfiable rules) or the error. The title shows the
status, e.g. `betbot alice: Spieltag 12 – 9 Tipps abgegeben`, `… – Warnungen`
or `betbot alice: Fehler`.

**When:** only if at least one tip was submitted, a warning occurred or the
tenant failed. A run with nothing to do (no open matches, everything already
tipped) sends nothing.

- With `--dry-run` nothing is sent; the message is logged as
  `dry run: would notify` instead.
- A notifier whose URL file is missing or invalid is disabled with a warning;
  a failed send (15 s limit) is logged. Neither changes the exit code.
  `--check-config` validates the URL files that exist (no network).
- A failed run is retried by Kubernetes (`backoffLimit`), so a persistent
  failure may be reported up to three times.

### Testing a notifier

`--test-notifiers` sends one German test message ("betbot: Testnachricht")
to every configured notifier, including notifiers no tenant references, and
exits. It does not log in to any platform, reads no credentials and submits
no tips. Unlike a normal run, failures are visible: each notifier is logged
as sent or failed (by name and kind, never the URL) and the exit code is
non-zero if any notifier failed. Run it locally with the same URL you put
into the Secret (write it into the file with an editor, so the token stays
out of your shell history):

```yaml
# config.yaml (a valid config still needs at least one tenant)
notifiers:
  - name: telegram-main
    urlFile: ./notifier-url.txt
tenants:
  - name: alice
    platform: { type: kicktipp, community: my-round-slug }
    notifiers: [telegram-main]
```

```sh
betbot --config config.yaml --test-notifiers
```

In the cluster the CronJob's arguments are fixed by the chart, so the test
runs locally; testing in the cluster would need a one-off pod with the
notifier Secret mounted.

## Command line

```text
betbot --config <FILE> [--dry-run | --check-config | --test-notifiers] [--log-format text|json]
```

| Flag | Env | Meaning |
| --- | --- | --- |
| `--config` | `BETBOT_CONFIG` | Path to the YAML config. |
| `--dry-run` | `BETBOT_DRY_RUN` | Compute and log the tips, but do not submit them. |
| `--check-config` | – | Validate the config and exit, without network access. |
| `--test-notifiers` | – | Send a test message to every configured notifier and exit. No platform access. |
| `--log-format` | `BETBOT_LOG_FORMAT` | `text` (default) or `json`. |
| – | `RUST_LOG` | Log filter, e.g. `info` (default) or `info,betbot=debug`. |

Exit codes: `0` all tenants succeeded, `1` at least one tenant failed or the
run was interrupted, `2` invalid config (Kubernetes does not retry it).
With `--test-notifiers`: `0` all test messages sent, `1` a send failed or
timed out (15 s), `2` no notifier configured or a URL file is missing,
unreadable or invalid.

## Deployment with Helm

Images (`linux/amd64`, `linux/arm64`) are published to
`ghcr.io/mrsteakhouse/betbot`, the chart to a Helm repository on GitHub Pages.

1. Create one Secret per tenant (the chart never manages credentials):

   ```sh
   kubectl create namespace betbot
   kubectl -n betbot create secret generic betbot-alice \
     --from-file=username=./username.txt \
     --from-file=password=./password.txt
   ```

   Optionally one Secret per notifier (key `url`), referenced in the values
   as `config.notifiers: [{ name: telegram-main, existingSecret: betbot-notify }]`:

   ```sh
   kubectl -n betbot create secret generic betbot-notify \
     --from-file=url=./notifier-url.txt
   ```

2. Write a values file. The `config` block is rendered into the ConfigMap; per
   tenant, `existingSecret` names the Secret, which is mounted at
   `/secrets/<tenant name>` (`credentialsDir` is set automatically):

   ```yaml
   dryRun: true   # first run: only log the tips
   config:
     tenants:
       - name: alice
         existingSecret: betbot-alice
         platform: { type: kicktipp, community: my-round-slug }
         scoring:
           homeWin: { tendency: 1, goalDifference: 2, exact: 3 }
           draw:    { tendency: 1, goalDifference: 2, exact: 3 }
           awayWin: { tendency: 1, goalDifference: 2, exact: 3 }
         rules:
           maxSameResult: { max: 6, scope: matchday, mirrored: true }
   ```

3. Install:

   ```sh
   helm repo add betbot https://mrsteakhouse.github.io/betbot
   helm repo update
   helm upgrade --install betbot betbot/betbot -n betbot -f values.yaml
   ```

4. Trigger a run once and check the log; if the tips look right, set
   `dryRun: false` and upgrade again:

   ```sh
   kubectl -n betbot create job --from=cronjob/betbot betbot-manual
   kubectl -n betbot logs -f job/betbot-manual
   kubectl -n betbot delete job betbot-manual
   ```

Important chart values (all of them with comments in
[`charts/betbot/values.yaml`](charts/betbot/values.yaml)):

| Value | Default | Meaning |
| --- | --- | --- |
| `schedule` / `timeZone` | `0 10 * * 1,5` / `Europe/Berlin` | Monday and Friday 10:00: before the weekend and before midweek rounds. |
| `dryRun` | `false` | Adds `--dry-run`. |
| `suspend` | `false` | Pause the CronJob. |
| `logLevel` | `info` | `RUST_LOG` filter. |
| `backoffLimit` | `2` | Retries after exit code 1; exit code 2 fails immediately. |
| `activeDeadlineSeconds` | `1800` | Total runtime limit of a Job. |
| `networkPolicy.enabled` | `false` | Allow only DNS and TCP 443 egress, no ingress. |
| `networkPolicy.extraEgress` | `[]` | Extra egress rules, e.g. for a self-hosted ntfy on another port. |
| `config.notifiers[].existingSecret` | – | Secret with the key `url`, mounted at `/notifiers/<name>/url` (`urlFile` is set automatically). |
| `image.tag` | chart `appVersion` | Image version. |

The pod runs as non-root (uid 65532) with a read-only root filesystem and no
capabilities.

## Development

The toolchain comes from [`flake.nix`](flake.nix) (Rust version pinned in
[`rust-toolchain.toml`](rust-toolchain.toml)):

```sh
nix develop   # or direnv allow
cargo run -- --config examples/config.yaml --dry-run   # offline demo tenant
cargo nextest run
cargo clippy --all-targets -- -D warnings
cargo deny check
helm lint charts/betbot -f charts/betbot/ci/full-values.yaml
```

Tests are hermetic: the kicktipp client is tested against a local mock server
with anonymised HTML fixtures in [`tests/fixtures/kicktipp/`](tests/fixtures/kicktipp/).
Never run tests or experiments against the real platform with real credentials.

To add a platform, odds provider or algorithm, implement the corresponding
trait (`src/platform/mod.rs`, `src/odds/mod.rs`, `src/algorithm/mod.rs`) and add
a variant to its config enum.

### Releases

Commits and PR titles follow [Conventional Commits](https://www.conventionalcommits.org/).
release-please opens a release PR from them. Merging it tags `vX.Y.Z`, builds
and pushes the multi-arch image and publishes the chart (chart version = app
version). Versions in `Cargo.toml` and `Chart.yaml` are never edited by hand.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
