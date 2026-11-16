# betbot Helm chart

Runs betbot, a tipping bot for Tippspiel rounds (kicktipp.de first), as a
Kubernetes CronJob. Each run reads the configured tenants, computes tips from
the betting odds and submits them for open matches. Credentials are read from
Secrets that you create yourself; the chart never handles them.

## Requirements

- Kubernetes >= 1.31 (`podFailurePolicy` is GA, CronJob `timeZone` is supported).
- Only if `networkPolicy.enabled: true`: a CNI/controller that enforces
  NetworkPolicy (k3s: the built-in kube-router does).

## Install

From the Helm repository:

```sh
helm repo add betbot https://mrsteakhouse.github.io/betbot
helm install betbot betbot/betbot -f my-values.yaml
```

From a local checkout:

```sh
helm install betbot charts/betbot -f my-values.yaml
```

The chart version always equals the app version; `appVersion` is the default
image tag (`ghcr.io/mrsteakhouse/betbot`). The chart is released together with
the app by release-please, so the versions in `Chart.yaml` are not edited by
hand.

`config.tenants` must contain at least one tenant; installing with the default
values fails schema validation on purpose.

## Credentials

Every tenant on a platform that needs a login (kicktipp) references a Secret
via `existingSecret`. The Secret must contain the keys `username` and
`password`:

```sh
kubectl create secret generic betbot-alice \
  --from-file=username=./username \
  --from-file=password=./password
```

One trailing newline in the files is tolerated. The Secret is mounted
read-only at `/secrets/<tenant name>` with mode 0440 and group 65532 (the pod's
`fsGroup`), and `credentialsDir` is set automatically in the rendered app
config. Credentials never belong in values.

## Values

| Key | Default | Description |
| --- | --- | --- |
| `image.repository` | `ghcr.io/mrsteakhouse/betbot` | Container image repository. |
| `image.tag` | `""` | Image tag; defaults to the chart `appVersion`. |
| `image.pullPolicy` | `IfNotPresent` | Image pull policy. |
| `imagePullSecrets` | `[]` | Pull secrets for private registries. |
| `nameOverride` | `""` | Overrides the chart name in resource names/labels. |
| `fullnameOverride` | `""` | Overrides the full resource name (max 52 characters). |
| `schedule` | `"0 10 * * 1,5"` | Cron schedule (Monday and Friday 10:00). |
| `timeZone` | `Europe/Berlin` | Time zone of `schedule`. |
| `suspend` | `false` | Suspends the CronJob. |
| `startingDeadlineSeconds` | `14400` | How late a missed run may still start. |
| `successfulJobsHistoryLimit` | `3` | Finished Jobs kept. |
| `failedJobsHistoryLimit` | `3` | Failed Jobs kept. |
| `activeDeadlineSeconds` | `1800` | Total runtime cap of a Job, including retries. |
| `backoffLimit` | `2` | Retries after exit code 1. |
| `dryRun` | `false` | Compute and log tips without submitting them (`--dry-run`). |
| `logLevel` | `info` | `RUST_LOG` filter, e.g. `info,betbot=debug`. |
| `resources` | requests 50m CPU / 32Mi, limit 128Mi | Container resources. |
| `podAnnotations` | `{}` | Extra pod annotations. |
| `podLabels` | `{}` | Extra pod labels. |
| `nodeSelector` | `{}` | Pod node selector. |
| `tolerations` | `[]` | Pod tolerations. |
| `affinity` | `{}` | Pod affinity. |
| `networkPolicy.enabled` | `false` | Creates a NetworkPolicy: no ingress, egress only DNS and TCP 443. |
| `networkPolicy.extraEgress` | `[]` | Additional egress rules, rendered verbatim (e.g. a self-hosted ntfy on another port). |
| `config.tenantTimeoutSeconds` | `300` | Time limit per tenant. |
| `config.tenants` | `[]` | Tenants, rendered 1:1 into the app config (required, at least one). |
| `config.tenants[].name` | - | Tenant name: `[A-Za-z0-9._-]`, max 63 characters, must start alphanumeric. |
| `config.tenants[].existingSecret` | - | Chart-only: Secret with `username`/`password`, mounted at `/secrets/<name>`. Sets `credentialsDir`; mutually exclusive with it. Removed from the app config. |
| `config.tenants[].notifiers` | `[]` | Names of notifiers (from `config.notifiers`) that receive the tenant's run summary. |
| `config.notifiers` | `[]` | Notification targets, see [Notifications](#notifications). Only rendered when non-empty. |
| `config.notifiers[].name` | - | Notifier name: same rules as tenant names. |
| `config.notifiers[].existingSecret` | - | Chart-only: Secret with the key `url`, mounted at `/notifiers/<name>/url`. Sets `urlFile`; exactly one of the two is required. Removed from the app config. |

All other tenant keys (`platform`, `odds`, `algorithm`, `scoring`, `rules`,
`overwriteExistingTips`, `credentialsDir`) are passed through to betbot
unchanged; see `examples/config.yaml` in the repository.

## Notifications

Each tenant can send one summary per run (matchday, submitted tips, warnings
or the error, in German) to Telegram, Discord, ntfy or a generic webhook. The
target is a shoutrrr-style URL that contains tokens, so it lives only in a
Secret you create (key `url`), never in values. Put the URL (e.g.
`telegram://<bot id>:<secret>@telegram?chats=<chat id>`) into a file, so the
token does not end up in your shell history:

```sh
kubectl create secret generic betbot-notify \
  --from-file=url=./notifier-url.txt
```

```yaml
config:
  notifiers:
    - name: telegram-main
      existingSecret: betbot-notify
  tenants:
    - name: alice
      existingSecret: betbot-alice
      platform: { type: kicktipp, community: my-round-slug }
      notifiers: [telegram-main]
```

URL formats:

- `telegram://<bot id>:<secret>@telegram?chats=@channel,123456[&notification=no]`
- `discord://<webhook token>@<webhook id>`
- `ntfy://[[user]:password@]host[:port]/topic[?scheme=http&priority=1..5&tags=a,b]`
- `generic://[user:password@]host[:port]/path[?disabletls=yes&template=json|plain&titlekey=title&messagekey=message]`

A summary is sent only when at least one tip was submitted, a warning
occurred or the tenant failed; a run with nothing to do sends nothing. With
`dryRun: true` nothing is sent, the message is only logged
(`dry run: would notify`). A missing or invalid URL disables that notifier
with a warning and a failed send is only logged; neither changes the exit
code. Since a failed Job is retried up to `backoffLimit` times, a persistent
failure may be reported up to `backoffLimit + 1` times.

With `networkPolicy.enabled: true` only TCP 443 is allowed; a self-hosted ntfy
or webhook on another port (`disabletls=yes`, `scheme=http`) needs a rule in
`networkPolicy.extraEgress`.

## Example values

Start with `dryRun: true`, check the logs, then switch it off.

```yaml
dryRun: true
config:
  tenants:
    - name: alice
      existingSecret: betbot-alice
      platform: { type: kicktipp, community: my-round-slug }
      odds: { type: platform }
      algorithm: { type: poisson, maxTipGoals: 5 }
      scoring:
        homeWin: { tendency: 1, goalDifference: 2, exact: 3 }
        draw: { tendency: 1, goalDifference: 2, exact: 3 }
        awayWin: { tendency: 1, goalDifference: 2, exact: 3 }
      rules:
        maxSameResult: { max: 6, scope: matchday, mirrored: true }
      overwriteExistingTips: false
```

## Run behaviour

- Exit code 0: all tenants succeeded.
- Exit code 1 (a tenant failed, or the run was aborted by a signal): the pod
  is retried up to `backoffLimit` times within `activeDeadlineSeconds`.
- Exit code 2 (invalid config): the Job fails immediately, without retries.
- Node drains and evictions (`DisruptionTarget`) do not count against
  `backoffLimit`.

With `overwriteExistingTips: true`, a retry re-submits that tenant's tips.
With the default `false`, existing tips are never touched, so retries and
repeated runs are harmless.

## Manual run

```sh
kubectl create job --from=cronjob/betbot betbot-manual
kubectl logs job/betbot-manual
kubectl delete job betbot-manual
```

The Job reuses the CronJob's arguments, so `--dry-run` is active only if
`dryRun: true` is set in the values.

## NetworkPolicy

With `networkPolicy.enabled: true` the pod gets no ingress and egress only to
DNS (`kube-system`/`kube-dns`, UDP/TCP 53) and TCP 443 anywhere, since
kicktipp has no fixed IP. "Anywhere" also covers in-cluster services on 443,
including the API server; this is accepted because the pod has no service
account token (`automountServiceAccountToken: false`).

## Validation

Render and inspect the app config, then let betbot validate it offline:

```sh
helm template betbot charts/betbot -f my-values.yaml -s templates/configmap.yaml \
  | yq -r '.data["config.yaml"]' > config.yaml
betbot --config config.yaml --check-config   # exit 0 valid, 2 invalid
```
