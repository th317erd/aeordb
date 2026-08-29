#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
gate="$repo_root/scripts/plan/check-v4-debt.sh"
contract_gate="$repo_root/scripts/plan/check-v4-contracts.sh"

fail() {
  printf 'check-v4-debt spec failed: %s\n' "$*" >&2
  exit 1
}

[[ -x "$gate" ]] || fail "gate is missing or not executable: $gate"
required_chain="\"\$repo_root/scripts/plan/check-v4-debt.sh\" \\"
rg -q --fixed-strings "$required_chain" "$contract_gate" \
  || fail "the independent v4 contract gate does not invoke the debt gate"

fixture=$(mktemp -d /tmp/codex/check-v4-debt-spec.XXXXXX)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/src"
printf 'TIMED_SHIM\nPERMANENT_PROJECTION\n' >"$fixture/src/allowed.rs"

baseline_policy="$fixture/policy.json"
jq -n '
  {
    schema_version: 1,
    campaign_id: "test-campaign",
    entries: [
      {
        id: "timed",
        classification: "timed_compatibility_shim",
        pattern: "TIMED_SHIM",
        scan_roots: ["src"],
        allowed_paths: ["src/allowed.rs"],
        maximum_matches: 1,
        owner: "test owner",
        rationale: "test timed compatibility",
        removal_gate: "remove after the test transition"
      },
      {
        id: "permanent",
        classification: "permanent_projection",
        pattern: "PERMANENT_PROJECTION",
        scan_roots: ["src"],
        allowed_paths: ["src/allowed.rs"],
        maximum_matches: 1,
        owner: "test owner",
        rationale: "test permanent projection",
        removal_gate: "replace only with a ratified public contract"
      },
      {
        id: "forbidden",
        classification: "forbidden",
        pattern: "DEAD_STUB",
        scan_roots: ["src"],
        allowed_paths: [],
        maximum_matches: 0,
        owner: "test owner",
        rationale: "dead test stub must remain absent",
        removal_gate: "never reintroduce without a new ratified implementation"
      }
    ]
  }
' >"$baseline_policy"

run_gate() {
  AEORDB_V4_DEBT_ROOT="$fixture" \
    AEORDB_V4_DEBT_POLICY="$1" \
    AEORDB_V4_DEBT_CAMPAIGN_ID="test-campaign" \
    timeout 5s "$gate"
}

expect_failure() {
  local name=$1
  local expected=$2
  local policy=$3
  local output="$fixture/$name.out"
  if run_gate "$policy" >"$output" 2>&1; then
    fail "$name unexpectedly passed"
  fi
  rg -q --fixed-strings "$expected" "$output" || fail "$name did not report '$expected'"
}

valid_output="$fixture/valid.out"
run_gate "$baseline_policy" >"$valid_output"
rg -q --fixed-strings 'v4 debt check: PASS (3 reviewed entries, 2 retained matches)' "$valid_output" \
  || fail "valid reviewed policy did not report its retained-match count"

missing_policy="$fixture/missing-policy.json"
expect_failure missing-policy "policy is missing: $missing_policy" "$missing_policy"

wrong_campaign="$fixture/wrong-campaign.json"
jq '.campaign_id = "other-campaign"' "$baseline_policy" >"$wrong_campaign"
expect_failure wrong-campaign "policy schema, campaign, or entries are invalid" "$wrong_campaign"

empty_id="$fixture/empty-id.json"
jq '(.entries[0].id) = ""' "$baseline_policy" >"$empty_id"
expect_failure empty-id "policy schema, campaign, or entries are invalid" "$empty_id"

missing_owner="$fixture/missing-owner.json"
jq '(.entries[] | select(.id == "timed") | .owner) = ""' "$baseline_policy" >"$missing_owner"
expect_failure missing-owner "entry 'timed' requires a non-empty owner" "$missing_owner"

invalid_classification="$fixture/invalid-classification.json"
jq '(.entries[] | select(.id == "timed") | .classification) = "mystery"' "$baseline_policy" >"$invalid_classification"
expect_failure invalid-classification "entry 'timed' has invalid classification: mystery" "$invalid_classification"

empty_pattern="$fixture/empty-pattern.json"
jq '(.entries[] | select(.id == "timed") | .pattern) = ""' "$baseline_policy" >"$empty_pattern"
expect_failure empty-pattern "entry 'timed' requires a non-empty single-line pattern" "$empty_pattern"

multiline_pattern="$fixture/multiline-pattern.json"
jq '(.entries[] | select(.id == "timed") | .pattern) = "TIMED\nSHIM"' "$baseline_policy" >"$multiline_pattern"
expect_failure multiline-pattern "entry 'timed' requires a non-empty single-line pattern" "$multiline_pattern"

missing_rationale="$fixture/missing-rationale.json"
jq '(.entries[] | select(.id == "timed") | .rationale) = ""' "$baseline_policy" >"$missing_rationale"
expect_failure missing-rationale "entry 'timed' requires a non-empty rationale" "$missing_rationale"

missing_removal="$fixture/missing-removal.json"
jq '(.entries[] | select(.id == "timed") | .removal_gate) = ""' "$baseline_policy" >"$missing_removal"
expect_failure missing-removal "entry 'timed' requires a non-empty removal_gate" "$missing_removal"

duplicate_id="$fixture/duplicate-id.json"
jq '.entries += [.entries[0]]' "$baseline_policy" >"$duplicate_id"
expect_failure duplicate-id "policy contains duplicate entry id: timed" "$duplicate_id"

invalid_pattern="$fixture/invalid-pattern.json"
jq '(.entries[] | select(.id == "timed") | .pattern) = "["' "$baseline_policy" >"$invalid_pattern"
expect_failure invalid-pattern "entry 'timed' has an invalid pattern or unreadable scan root" "$invalid_pattern"

invalid_maximum="$fixture/invalid-maximum.json"
jq '(.entries[] | select(.id == "timed") | .maximum_matches) = "one"' "$baseline_policy" >"$invalid_maximum"
expect_failure invalid-maximum "entry 'timed' requires a non-negative integer maximum_matches" "$invalid_maximum"

forbidden_nonzero="$fixture/forbidden-nonzero.json"
jq '(.entries[] | select(.id == "forbidden") | .maximum_matches) = 1' "$baseline_policy" >"$forbidden_nonzero"
expect_failure forbidden-nonzero "forbidden entry 'forbidden' must set maximum_matches to 0" "$forbidden_nonzero"

retained_zero="$fixture/retained-zero.json"
jq '(.entries[] | select(.id == "timed") | .maximum_matches) = 0' "$baseline_policy" >"$retained_zero"
expect_failure retained-zero "retained entry 'timed' must set maximum_matches above zero" "$retained_zero"

empty_scan_roots="$fixture/empty-scan-roots.json"
jq '(.entries[] | select(.id == "timed") | .scan_roots) = []' "$baseline_policy" >"$empty_scan_roots"
expect_failure empty-scan-roots "entry 'timed' requires non-empty scan_roots" "$empty_scan_roots"

invalid_allowed_paths="$fixture/invalid-allowed-paths.json"
jq '(.entries[] | select(.id == "timed") | .allowed_paths) = null' "$baseline_policy" >"$invalid_allowed_paths"
expect_failure invalid-allowed-paths "entry 'timed' requires an allowed_paths array" "$invalid_allowed_paths"

duplicate_scan_root="$fixture/duplicate-scan-root.json"
jq '(.entries[] | select(.id == "timed") | .scan_roots) = ["src", "src"]' "$baseline_policy" >"$duplicate_scan_root"
expect_failure duplicate-scan-root "entry 'timed' repeats a scan root" "$duplicate_scan_root"

duplicate_allowed_path="$fixture/duplicate-allowed-path.json"
jq '(.entries[] | select(.id == "timed") | .allowed_paths) = ["src/allowed.rs", "src/allowed.rs"]' "$baseline_policy" >"$duplicate_allowed_path"
expect_failure duplicate-allowed-path "entry 'timed' repeats an allowed path" "$duplicate_allowed_path"

forbidden_allowed_path="$fixture/forbidden-allowed-path.json"
jq '(.entries[] | select(.id == "forbidden") | .allowed_paths) = ["src/allowed.rs"]' "$baseline_policy" >"$forbidden_allowed_path"
expect_failure forbidden-allowed-path "forbidden entry 'forbidden' must not allow paths" "$forbidden_allowed_path"

missing_scan_root="$fixture/missing-scan-root.json"
jq '(.entries[] | select(.id == "timed") | .scan_roots) = ["missing"]' "$baseline_policy" >"$missing_scan_root"
expect_failure missing-scan-root "entry 'timed' scan root is missing: missing" "$missing_scan_root"

printf 'TIMED_SHIM\n' >"$fixture/src/unreviewed.rs"
expect_failure unreviewed-path "entry 'timed' matched outside allowed_paths: src/unreviewed.rs" "$baseline_policy"
rm -f "$fixture/src/unreviewed.rs"

printf 'TIMED_SHIM\n' >>"$fixture/src/allowed.rs"
expect_failure match-growth "entry 'timed' match count 2 exceeds maximum_matches 1" "$baseline_policy"
sed -i '$d' "$fixture/src/allowed.rs"

printf 'DEAD_STUB\n' >>"$fixture/src/allowed.rs"
expect_failure forbidden-match "forbidden entry 'forbidden' matched: src/allowed.rs" "$baseline_policy"
sed -i '$d' "$fixture/src/allowed.rs"

stale_allowed="$fixture/stale-allowed.json"
jq '(.entries[] | select(.id == "timed") | .allowed_paths) = ["src/missing.rs"]' "$baseline_policy" >"$stale_allowed"
expect_failure stale-allowed "entry 'timed' allowed path is missing: src/missing.rs" "$stale_allowed"

unsafe_root="$fixture/unsafe-root.json"
jq '(.entries[] | select(.id == "timed") | .scan_roots) = ["../escape"]' "$baseline_policy" >"$unsafe_root"
expect_failure unsafe-root "entry 'timed' has unsafe scan root: ../escape" "$unsafe_root"

unsafe_allowed_path="$fixture/unsafe-allowed-path.json"
jq '(.entries[] | select(.id == "timed") | .allowed_paths) = ["../escape.rs"]' "$baseline_policy" >"$unsafe_allowed_path"
expect_failure unsafe-allowed-path "entry 'timed' has unsafe allowed path: ../escape.rs" "$unsafe_allowed_path"

outside_scan="$fixture/outside-scan.json"
jq '(.entries[] | select(.id == "timed") | .scan_roots) = ["src/unrelated.rs"]' "$baseline_policy" >"$outside_scan"
printf 'NO_DEBT\n' >"$fixture/src/unrelated.rs"
expect_failure outside-scan "entry 'timed' allowed path is outside scan_roots: src/allowed.rs" "$outside_scan"

malformed="$fixture/malformed.json"
printf '{not-json\n' >"$malformed"
expect_failure malformed-policy "policy is not valid JSON" "$malformed"

printf 'check-v4-debt self-test: PASS\n'
