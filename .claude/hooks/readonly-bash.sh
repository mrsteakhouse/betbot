#!/usr/bin/env bash
# PreToolUse hook for read-only subagents (planner, reviewer, tester).
#
#   readonly-bash.sh           planner, reviewer: strictly read-only
#   readonly-bash.sh --tester  tester: read-only, plus throwaway test pods
#                              (kubectl run/delete of busybox pods named
#                              claude-test-*) and curl GET/HEAD
#
# Allowlist, fail-closed: every segment of a Bash command (split on ; & | and
# newlines, quote-aware) must be a known read-only command. Anything that is
# not explicitly allowed is blocked. Exit code 2 blocks the tool call and
# hands stderr back to the agent.
#
# Rust/Nix/chart checks (cargo check/clippy/test/nextest/fmt --check/deny check,
# nix flake show|metadata|check, nix eval, nix develop -c <allowed cmd>, helm lint,
# ct lint) are allowed. They write only build artifacts (target/, /nix/store) but
# DO execute repo code (build.rs, proc macros, tests, shellHook): the hook guards
# against agent mistakes, not against a malicious repo.
set -u

tester=0
[[ "${1:-}" == "--tester" ]] && tester=1

block() {
  echo "Blocked by read-only hook: $1" >&2
  echo "This agent is read-only. Allowed: git log/show/diff/status/..., cargo metadata/tree/check/clippy/test/nextest run|list/fmt --check/deny check/--version, rustc --version, nix flake show|metadata|check and nix eval and nix develop -c <allowed cmd> (all with --no-write-lock-file), helm show/template/lint, ct lint --skip-helm-dependencies, actionlint, kubectl get/describe/logs/wait/rollout status/... (apply/create only with --dry-run; no secret values), ls/cat/grep/find/jq/yq/sed (without writing)." >&2
  ((tester)) && echo "Tester extras: kubectl run claude-test-<name> --image=busybox[:tag] --restart=Never (no --privileged/--expose, overrides without host*/privileged/serviceAccount), kubectl delete pod claude-test-<name> or -l <selector containing claude-test=true>, curl GET/HEAD without writing files." >&2
  exit 2
}

# Tester only: kubectl run of a throwaway busybox pod.
check_test_run() {
  local seg=$1
  [[ "$seg" =~ run[[:space:]]+claude-test-[a-z0-9-]+([[:space:]]|$) ]] || block "kubectl run: pod name must start with claude-test-"
  [[ "$seg" =~ --image(=|[[:space:]]+)[\"\']?(docker\.io/(library/)?)?busybox(:[A-Za-z0-9._-]+)?[\"\']?([[:space:]]|$) ]] || block "kubectl run: only --image=busybox[:tag]"
  [[ "$seg" =~ --restart(=|[[:space:]]+)Never ]] || block "kubectl run: --restart=Never required"
  [[ "$seg" =~ --(privileged|expose|port|serviceaccount)([=[:space:]]|$) ]] && block "kubectl run: --privileged/--expose/--port/--serviceaccount not allowed"
  [[ "$seg" =~ (hostNetwork|hostPID|hostIPC|hostPath|hostPort|privileged|allowPrivilegeEscalation\"?:[[:space:]]*true|serviceAccount|automountServiceAccountToken\"?:[[:space:]]*true|nodeName|capabilities|\"volumes\") ]] &&
    block "kubectl run: overrides with host access, privileges, volumes, capabilities or service accounts are not allowed"
  return 0
}

# Tester only: kubectl delete of throwaway test pods.
check_test_delete() {
  local seg=$1 w a skip=0 type="" names=0
  read -r -a w <<<"$seg"
  [[ "$seg" =~ [[:space:]]--all([[:space:]]|$) ]] && block "kubectl delete --all"
  for a in "${w[@]:1}"; do
    if ((skip)); then skip=0; continue; fi
    case "$a" in
      -n | --namespace | -l | --selector | --context | --kubeconfig | --grace-period | --timeout) skip=1; continue ;;
      -*) continue ;;
      delete) continue ;;
    esac
    if [[ -z "$type" ]]; then
      type=$a
      [[ "$type" =~ ^(pod|pods|po)$ ]] || [[ "$type" =~ ^(pod|pods|po)/claude-test-[a-z0-9-]+$ ]] || block "kubectl delete: only pods"
      [[ "$type" == */* ]] && names=$((names + 1))
      continue
    fi
    [[ "$a" =~ ^claude-test-[a-z0-9-]+$ ]] || block "kubectl delete: pod name must start with claude-test-"
    names=$((names + 1))
  done
  [[ -n "$type" ]] || block "kubectl delete: resource type missing"
  if ((names == 0)); then
    [[ "$seg" =~ (-l|--selector)(=|[[:space:]]+)[\"\']?([A-Za-z0-9./_-]+=[A-Za-z0-9._-]+,)*claude-test=true([,\"\'[:space:]]|$) ]] ||
      block "kubectl delete: needs claude-test-* names or a selector containing claude-test=true"
  fi
  return 0
}

input=$(cat) || block "could not read hook input"
tool=$(jq -r '.tool_name // empty' <<<"$input" 2>/dev/null) || block "invalid hook input"
[[ "$tool" == "Bash" ]] || exit 0
cmd=$(jq -r '.tool_input.command // empty' <<<"$input" 2>/dev/null) || block "invalid hook input"
[[ -n "$cmd" ]] || block "empty command"

# No command/process substitution anywhere, not even inside quotes.
[[ "$cmd" == *'$('* || "$cmd" == *'`'* || "$cmd" == *'<('* || "$cmd" == *'>('* ]] &&
  block "command or process substitution is not allowed"

# Harmless redirections are removed; any other '>' outside quotes is blocked below.
cmd=$(sed -E 's/[[:space:]]*([0-9]|&)?>[[:space:]]*\/dev\/null//g; s/[[:space:]]*[0-9]>&[0-9]//g' <<<"$cmd")

# Quote-aware split into segments.
segments=()
cur="" sq=0 dq=0
for ((i = 0; i < ${#cmd}; i++)); do
  c=${cmd:i:1}
  if ((sq)); then
    [[ "$c" == "'" ]] && sq=0
    cur+=$c
    continue
  fi
  if ((dq)); then
    if [[ "$c" == '\' ]]; then cur+=$c${cmd:i+1:1}; ((i++)); continue; fi
    [[ "$c" == '"' ]] && dq=0
    cur+=$c
    continue
  fi
  case "$c" in
    "'") sq=1; cur+=$c ;;
    '"') dq=1; cur+=$c ;;
    '\') cur+=$c${cmd:i+1:1}; ((i++)) ;;
    '>') block "output redirection is not allowed" ;;
    ';' | '&' | '|' | $'\n') segments+=("$cur"); cur="" ;;
    *) cur+=$c ;;
  esac
done
((sq || dq)) && block "unbalanced quotes"
segments+=("$cur")

# First positional argument after the tool name, skipping global flags.
# $1 = space-separated list of flags that take a value.
subcommand() {
  local valflags=" $1 " w skip=0
  shift
  for w in "$@"; do
    if ((skip)); then skip=0; continue; fi
    if [[ "$w" == -* ]]; then
      [[ "$valflags" == *" $w "* ]] && skip=1
      continue
    fi
    echo "$w"
    return
  done
}

# Elements of "$@" after the first occurrence of word $1 -> global array sub_args.
sub_args=()
set_sub_args() {
  local word=$1 a found=0
  shift
  sub_args=()
  for a in "$@"; do
    if ((found)); then sub_args+=("$a"); elif [[ "$a" == "$word" ]]; then found=1; fi
  done
}

# True if every argument is a version flag (and there is at least one).
only_version() {
  local a
  (($# > 0)) || return 1
  for a in "$@"; do
    [[ "$a" == -V || "$a" == --version || "$a" == -vV ]] || return 1
  done
}

check_cargo_flags() {
  [[ "$1" =~ [[:space:]](--config|--fix|--allow-dirty|--allow-staged|--allow-no-vcs|--target-dir|--artifact-dir|--out-dir|--lockfile-path|--open)([=[:space:]]|$) ]] &&
    block "cargo ${BASH_REMATCH[1]} (can run commands, rewrite sources or write outside target/)"
  [[ "$1" =~ [[:space:]]-Z ]] && block "cargo -Z (unstable flags)"
  return 0
}

check_nix_flags() {
  [[ "$1" =~ [[:space:]](--option|--accept-flake-config|--allow-unsafe-native-code-during-evaluation|--update-input|--recreate-lock-file|--commit-lock-file|--write-to|--out-link|-o|--profile|--builders|--store|--eval-store)([=[:space:]]|$) ]] &&
    block "nix ${BASH_REMATCH[1]} (can run commands or write outside the store)"
  [[ "$1" =~ [[:space:]]--[a-z-]*-hook([=[:space:]]|$) ]] && block "nix *-hook settings run commands"
  return 0
}

require_nolock() {
  [[ "$1" =~ [[:space:]]--no-(write|update)-lock-file([[:space:]]|$) ]] ||
    block "nix: add --no-write-lock-file (or --no-update-lock-file) so flake.lock cannot change"
}

check_segment() {
  local seg=$1 w
  read -r -a w <<<"$seg"
  ((${#w[@]})) || return 0
  # Leading environment assignments: only KUBECONFIG (others like GIT_PAGER can run commands).
  while ((${#w[@]})) && [[ "${w[0]}" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; do
    [[ "${w[0]}" == KUBECONFIG=* ]] || block "environment assignment ${w[0]%%=*}"
    w=("${w[@]:1}")
  done
  ((${#w[@]})) || return 0

  local prog=${w[0]} args=("${w[@]:1}") sub
  case "$prog" in
    git)
      sub=$(subcommand "-C -c --git-dir --work-tree" "${args[@]}")
      case "$sub" in
        log | show | diff | status | blame | ls-files | ls-tree | rev-parse | grep | shortlog | describe | cat-file | merge-base | rev-list) ;;
        *) block "git $sub" ;;
      esac
      [[ "$seg" == *--output* ]] && block "git --output writes files"
      [[ " ${args[*]} " =~ [[:space:]]-c[[:space:]] ]] && block "git -c can set pager/external commands"
      [[ "$seg" =~ [[:space:]](-O|--open-files-in-pager) ]] && block "git grep -O runs a pager"
      ;;
    kubectl)
      sub=$(subcommand "-n --namespace --context --kubeconfig --cluster --user -l --selector -o --output" "${args[@]}")
      if [[ "$sub" == get ]]; then
        local secret_re='(^|[[:space:],/])secrets?([[:space:],/.]|$)'
        local fmt_re="[[:space:]](-o|--output)[=[:space:]]*[\"']?(json|yaml|jsonpath|go-template|custom-columns|template)"
        if [[ "$seg" =~ $secret_re && "$seg" =~ $fmt_re ]]; then
          block "kubectl get secret with a data-printing output format (use describe or -o name)"
        fi
        [[ "$seg" =~ [[:space:]]--raw([=[:space:]]|$) && "$seg" == *secret* ]] && block "kubectl get --raw on secrets"
      fi
      case "$sub" in
        get | describe | logs | explain | top | api-resources | api-versions | version | cluster-info | events | diff | wait) ;;
        rollout)
          [[ "$seg" =~ rollout[[:space:]]+(status|history)([[:space:]]|$) ]] || block "kubectl rollout (only status/history)"
          ;;
        run)
          ((tester)) || block "kubectl run"
          check_test_run "$seg"
          ;;
        delete)
          ((tester)) || block "kubectl delete"
          check_test_delete "$seg"
          ;;
        apply | create)
          [[ "$seg" =~ --dry-run(=|[[:space:]]+)(server|client) ]] || block "kubectl $sub without --dry-run=server|client"
          ;;
        auth)
          [[ "$seg" =~ auth[[:space:]]+(can-i|whoami) ]] || block "kubectl auth (only can-i/whoami)"
          ;;
        config)
          [[ "$seg" =~ config[[:space:]]+(view|get-contexts|current-context) ]] || block "kubectl config (only view/get-contexts/current-context)"
          ;;
        *) block "kubectl $sub" ;;
      esac
      ;;
    helm)
      sub=$(subcommand "-n --namespace --kube-context --kubeconfig" "${args[@]}")
      case "$sub" in
        show | inspect | template | search | list | ls | get | status | history | version | lint | env) ;;
        repo | dependency | dep)
          [[ "$seg" =~ (repo|dependency|dep)[[:space:]]+(list|ls) ]] || block "helm $sub (only list)"
          ;;
        *) block "helm $sub" ;;
      esac
      [[ "$seg" == *--output-dir* ]] && block "helm --output-dir writes files"
      ;;
    cargo)
      sub=$(subcommand "--color" "${args[@]}")
      check_cargo_flags "$seg"
      case "$sub" in
        metadata | tree | check | clippy | test | version) ;;
        fmt)
          [[ "$seg" =~ [[:space:]]--check([[:space:]]|$) ]] || block "cargo fmt only with --check (no writing)"
          [[ "$seg" =~ [[:space:]]--(emit|print-config)([=[:space:]]|$) ]] && block "cargo fmt --emit/--print-config can write files"
          ;;
        nextest)
          set_sub_args nextest "${args[@]}"
          case "$(subcommand "--manifest-path --color --profile -P" "${sub_args[@]}")" in
            run | list) ;;
            *) block "cargo nextest (only run/list)" ;;
          esac
          ;;
        deny)
          set_sub_args deny "${args[@]}"
          case "$(subcommand "-L --log-level -f --format --color --manifest-path -c --target" "${sub_args[@]}")" in
            check) ;;
            *) block "cargo deny (only check)" ;;
          esac
          ;;
        "")
          only_version "${args[@]}" || block "cargo without subcommand (only --version)"
          ;;
        *) block "cargo $sub (allowed: metadata, tree, check, clippy, test, nextest run|list, fmt --check, deny check)" ;;
      esac
      ;;
    rustc)
      only_version "${args[@]}" || block "rustc (only --version)"
      ;;
    nix)
      check_nix_flags "$seg"
      sub=$(subcommand "" "${args[@]}")
      case "$sub" in
        flake)
          set_sub_args flake "${args[@]}"
          case "$(subcommand "" "${sub_args[@]}")" in
            show | metadata | check | info) require_nolock "$seg" ;;
            *) block "nix flake (only show/metadata/check)" ;;
          esac
          ;;
        eval)
          require_nolock "$seg"
          ;;
        develop)
          require_nolock "$seg"
          local i cmdstart=-1 nested
          for i in "${!w[@]}"; do
            if [[ "${w[i]}" == -c || "${w[i]}" == --command ]]; then cmdstart=$i; break; fi
          done
          ((cmdstart >= 0)) || block "nix develop without -c <command> (would open an interactive shell)"
          nested="${w[*]:cmdstart+1}"
          [[ -n "${nested// /}" ]] || block "nix develop -c without a command"
          check_segment "$nested"
          ;;
        "")
          only_version "${args[@]}" || block "nix without subcommand (only --version)"
          ;;
        *) block "nix $sub (allowed: flake show|metadata|check, eval, develop -c <cmd>)" ;;
      esac
      ;;
    ct)
      sub=$(subcommand "--config" "${args[@]}")
      case "$sub" in
        lint)
          [[ "$seg" =~ [[:space:]]--skip-helm-dependencies([=[:space:]]|$) ]] ||
            block "ct lint: add --skip-helm-dependencies (dependency build writes charts/ and Chart.lock and needs network)"
          [[ "$seg" =~ [[:space:]]--(chart-repos|helm-repo-extra-args)([=[:space:]]|$) ]] && block "ct lint --chart-repos/--helm-repo-extra-args run helm repo add"
          ;;
        list-changed | version) ;;
        *) block "ct $sub (only lint/list-changed/version)" ;;
      esac
      ;;
    actionlint)
      [[ "$seg" =~ [[:space:]]-(shellcheck|pyflakes)([=[:space:]]|$) ]] && block "actionlint -shellcheck/-pyflakes execute the given path"
      ;;
    find)
      [[ "$seg" =~ [[:space:]]-(exec|execdir|ok|okdir|delete|fprint|fprint0|fprintf|fls)([[:space:]]|$) ]] && block "find with an executing/writing action"
      ;;
    sed)
      [[ "$seg" =~ [[:space:]](-i|--in-place|-[a-zA-Z]*i) ]] && block "sed -i"
      local sed_write="(^|[^A-Za-z])[wWe]([[:space:]]|'|\$)"
      [[ "$seg" =~ $sed_write ]] && block "sed w/W/e commands"
      ;;
    yq)
      [[ "$seg" =~ [[:space:]](-i|--inplace|-[a-zA-Z]*i)([[:space:]]|$) ]] && block "yq -i"
      ;;
    sort)
      [[ "$seg" =~ [[:space:]](-o|--output) ]] && block "sort -o"
      ;;
    tree)
      [[ "$seg" =~ [[:space:]]-o([[:space:]]|$) ]] && block "tree -o"
      ;;
    uniq)
      n=0; for a in "${args[@]}"; do [[ "$a" == -* ]] || ((n++)); done
      ((n <= 1)) || block "uniq with an output file"
      ;;
    curl)
      ((tester)) || block "curl (use WebFetch)"
      seg=$(sed -E 's/[[:space:]](-o|--output)[[:space:]]+\/dev\/null([[:space:]]|$)/ /g' <<<"$seg")
      [[ "$seg" =~ [[:space:]](-X|--request|-d|--data[a-z-]*|--json|-F|--form[a-z-]*|-T|--upload-file|-o|--output|-O|--remote-name[a-z-]*|-K|--config|-c|--cookie-jar|-D|--dump-header|--stderr|--trace[a-z-]*|--libcurl|--etag-save|--hsts|--alt-svc)([=[:space:]]|$) ]] &&
        block "curl: only GET/HEAD to stdout (no request body, method override or file output)"
      [[ "$seg" =~ [[:space:]]-[A-Za-z]*[XdFTKOocD] ]] && block "curl: combined short flags with -X/-d/-F/-T/-K/-O/-o/-c/-D are not allowed"
      ;;
    ls | cat | head | tail | wc | grep | rg | jq | cut | tr | diff | stat | file | echo | printf | pwd | cd | \
      basename | dirname | realpath | readlink | column | comm | base64 | true | false | test | '[' | which | date | nl) ;;
    *) block "'$prog' is not on the read-only allowlist" ;;
  esac
}

for s in "${segments[@]}"; do
  check_segment "$s"
done
exit 0
