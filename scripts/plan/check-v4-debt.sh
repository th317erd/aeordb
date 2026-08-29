#!/usr/bin/env bash
set -euo pipefail

default_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
repo_root=${AEORDB_V4_DEBT_ROOT:-$default_root}
policy=${AEORDB_V4_DEBT_POLICY:-$repo_root/bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor/evidence/v4-debt-policy.json}
campaign_id=${AEORDB_V4_DEBT_CAMPAIGN_ID:-aeordb-v4-nvt-gc-2026-08-03}

fail() {
  printf 'v4 debt check failed: %s\n' "$*" >&2
  exit 1
}

safe_relative_path() {
  local path=$1
  [[ -n "$path" && "$path" != /* && "$path" != .* && "$path" != *\\* && "$path" != */../* && "$path" != */.. ]]
}

for tool in jq rg sort wc; do
  command -v "$tool" >/dev/null 2>&1 || fail "required tool '$tool' is unavailable"
done

[[ -d "$repo_root" ]] || fail "repository root is missing: $repo_root"
[[ -f "$policy" ]] || fail "policy is missing: $policy"
jq empty "$policy" >/dev/null 2>&1 || fail "policy is not valid JSON: $policy"

jq -e --arg campaign "$campaign_id" '
  .schema_version == 1 and
  .campaign_id == $campaign and
  (.entries | type == "array" and length > 0) and
  all(.entries[]; .id | type == "string" and length > 0)
' "$policy" >/dev/null || fail "policy schema, campaign, or entries are invalid"

duplicate_ids=$(jq -r '.entries | group_by(.id)[] | select(length > 1) | .[0].id' "$policy")
[[ -z "$duplicate_ids" ]] || fail "policy contains duplicate entry id: $(printf '%s' "$duplicate_ids" | head -n 1)"

mapfile -t entry_ids < <(jq -r '.entries[].id' "$policy")
retained_matches=0

for id in "${entry_ids[@]}"; do
  entry=$(jq -c --arg id "$id" '.entries[] | select(.id == $id)' "$policy")
  classification=$(jq -r '.classification // ""' <<<"$entry")
  pattern=$(jq -r '.pattern // ""' <<<"$entry")
  owner=$(jq -r '.owner // ""' <<<"$entry")
  rationale=$(jq -r '.rationale // ""' <<<"$entry")
  removal_gate=$(jq -r '.removal_gate // ""' <<<"$entry")
  maximum_matches=$(jq -r '.maximum_matches // "invalid"' <<<"$entry")

  case "$classification" in
    timed_compatibility_shim | permanent_projection | forbidden) ;;
    *) fail "entry '$id' has invalid classification: $classification" ;;
  esac
  [[ -n "$pattern" && "$pattern" != *$'\n'* ]] || fail "entry '$id' requires a non-empty single-line pattern"
  [[ -n "$owner" ]] || fail "entry '$id' requires a non-empty owner"
  [[ -n "$rationale" ]] || fail "entry '$id' requires a non-empty rationale"
  [[ -n "$removal_gate" ]] || fail "entry '$id' requires a non-empty removal_gate"
  [[ "$maximum_matches" =~ ^[0-9]+$ ]] || fail "entry '$id' requires a non-negative integer maximum_matches"
  if [[ "$classification" == forbidden ]]; then
    [[ "$maximum_matches" == 0 ]] || fail "forbidden entry '$id' must set maximum_matches to 0"
  elif ((maximum_matches == 0)); then
    fail "retained entry '$id' must set maximum_matches above zero"
  fi

  jq -e '.scan_roots | type == "array" and length > 0 and all(.[]; type == "string" and length > 0)' <<<"$entry" >/dev/null \
    || fail "entry '$id' requires non-empty scan_roots"
  jq -e '.allowed_paths | type == "array" and all(.[]; type == "string" and length > 0)' <<<"$entry" >/dev/null \
    || fail "entry '$id' requires an allowed_paths array"
  [[ "$(jq '[.scan_roots[]] | length == (unique | length)' <<<"$entry")" == true ]] || fail "entry '$id' repeats a scan root"
  [[ "$(jq '[.allowed_paths[]] | length == (unique | length)' <<<"$entry")" == true ]] || fail "entry '$id' repeats an allowed path"

  mapfile -t scan_roots < <(jq -r '.scan_roots[]' <<<"$entry")
  mapfile -t allowed_paths < <(jq -r '.allowed_paths[]' <<<"$entry")
  if [[ "$classification" == forbidden && ${#allowed_paths[@]} -ne 0 ]]; then
    fail "forbidden entry '$id' must not allow paths"
  fi

  for scan_root in "${scan_roots[@]}"; do
    safe_relative_path "$scan_root" || fail "entry '$id' has unsafe scan root: $scan_root"
    [[ -e "$repo_root/$scan_root" ]] || fail "entry '$id' scan root is missing: $scan_root"
  done
  for allowed_path in "${allowed_paths[@]}"; do
    safe_relative_path "$allowed_path" || fail "entry '$id' has unsafe allowed path: $allowed_path"
    [[ -f "$repo_root/$allowed_path" ]] || fail "entry '$id' allowed path is missing: $allowed_path"
    covered=false
    for scan_root in "${scan_roots[@]}"; do
      if [[ "$allowed_path" == "$scan_root" || "$allowed_path" == "$scan_root/"* ]]; then
        covered=true
        break
      fi
    done
    [[ "$covered" == true ]] || fail "entry '$id' allowed path is outside scan_roots: $allowed_path"
  done

  matches=$(mktemp)
  set +e
  (
    cd "$repo_root"
    rg --line-number --with-filename --no-heading --color never \
      --glob '*.rs' --glob '*.toml' --glob '*.md' --glob '*.json' --glob '*.sh' \
      -e "$pattern" -- "${scan_roots[@]}"
  ) >"$matches" 2>/dev/null
  rg_code=$?
  set -e
  if [[ "$rg_code" -ne 0 && "$rg_code" -ne 1 ]]; then
    rm -f "$matches"
    fail "entry '$id' has an invalid pattern or unreadable scan root"
  fi
  sort -u -o "$matches" "$matches"
  match_count=$(wc -l <"$matches" | tr -d '[:space:]')

  if [[ "$classification" == forbidden ]]; then
    if ((match_count > 0)); then
      first_path=$(head -n 1 "$matches")
      first_path=${first_path%%:*}
      rm -f "$matches"
      fail "forbidden entry '$id' matched: $first_path"
    fi
    rm -f "$matches"
    continue
  fi

  unreviewed_path=
  while IFS= read -r match; do
    [[ -n "$match" ]] || continue
    match_path=${match%%:*}
    reviewed=false
    for allowed_path in "${allowed_paths[@]}"; do
      if [[ "$match_path" == "$allowed_path" ]]; then
        reviewed=true
        break
      fi
    done
    if [[ "$reviewed" != true ]]; then
      unreviewed_path=$match_path
      break
    fi
  done <"$matches"
  if [[ -n "$unreviewed_path" ]]; then
    rm -f "$matches"
    fail "entry '$id' matched outside allowed_paths: $unreviewed_path"
  fi

  if ((match_count > maximum_matches)); then
    rm -f "$matches"
    fail "entry '$id' match count $match_count exceeds maximum_matches $maximum_matches"
  fi

  for allowed_path in "${allowed_paths[@]}"; do
    if ! awk -F: -v path="$allowed_path" '$1 == path { found=1 } END { exit(found ? 0 : 1) }' "$matches"; then
      rm -f "$matches"
      fail "entry '$id' allowed path has no current match: $allowed_path"
    fi
  done
  retained_matches=$((retained_matches + match_count))
  rm -f "$matches"
done

printf 'v4 debt check: PASS (%s reviewed entries, %s retained matches)\n' "${#entry_ids[@]}" "$retained_matches"
