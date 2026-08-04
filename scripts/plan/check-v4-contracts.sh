#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
evidence_dir="$repo_root/bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor/evidence"
campaign_id="aeordb-v4-nvt-gc-2026-08-03"

fail() {
  printf 'v4 contract check failed: %s\n' "$*" >&2
  exit 1
}

for tool in cargo git jq rg python3; do
  command -v "$tool" >/dev/null 2>&1 || fail "required tool '$tool' is unavailable"
done

json_files=(
  baseline-environment.json
  baseline-behavior-and-performance.json
  persisted-producer-consumer-inventory.json
  recent-fix-ledger.json
  route-root-contract-manifest.json
)

for name in "${json_files[@]}"; do
  path="$evidence_dir/$name"
  [[ -f "$path" ]] || fail "missing evidence file $name"
  jq -e --arg campaign "$campaign_id" '.schema_version == 1 and .campaign_id == $campaign' "$path" >/dev/null \
    || fail "$name has an invalid schema or campaign id"
done

divergences="$evidence_dir/intended-divergences.yaml"
[[ -f "$divergences" ]] || fail "missing evidence file intended-divergences.yaml"
python3 - "$divergences" <<'PY'
import sys

import yaml

with open(sys.argv[1], encoding="utf-8") as source:
    document = yaml.safe_load(source)
if document.get("schema_version") != 1:
    raise SystemExit("invalid divergence schema")
entries = document.get("allowed_divergences", [])
ids = [entry["id"] for entry in entries]
if len(ids) != len(set(ids)):
    raise SystemExit("duplicate divergence id")
if not entries or not all(entry.get("proof") for entry in entries):
    raise SystemExit("empty divergence proof")
if not document.get("forbidden_divergences"):
    raise SystemExit("missing forbidden divergences")
PY

entry_commit=$(jq -r '.source.entry_commit' "$evidence_dir/baseline-environment.json")
git -C "$repo_root" cat-file -e "$entry_commit^{commit}" 2>/dev/null || fail "entry commit $entry_commit is not in repository history"

behavior_commit=$(jq -r '.source_commit' "$evidence_dir/baseline-behavior-and-performance.json")
inventory_commit=$(jq -r '.source_commit' "$evidence_dir/persisted-producer-consumer-inventory.json")
route_commit=$(jq -r '.source_commit' "$evidence_dir/route-root-contract-manifest.json")
[[ "$entry_commit" == "$behavior_commit" && "$entry_commit" == "$inventory_commit" && "$entry_commit" == "$route_commit" ]] \
  || fail "P0a source commits disagree"

jq -e '
  .characterization.run_2.result == "pass" and
  .characterization.run_3.result == "pass" and
  .characterization.run_2.failed == 0 and
  .characterization.run_3.failed == 0 and
  .characterization.run_2.normalized_result_sha256 == .characterization.run_3.normalized_result_sha256 and
  (.focused_probes | all(.result == "pass"))
' "$evidence_dir/baseline-behavior-and-performance.json" >/dev/null || fail "behavior characterization is not green and equivalent"

route_source_count=$(rg -c '\.route\s*\(' "$repo_root/aeordb-lib/src/server/mod.rs")
manifest_route_count=$(jq '.route_registration_count' "$evidence_dir/route-root-contract-manifest.json")
group_route_count=$(jq '[.route_groups[].paths | length] | add' "$evidence_dir/route-root-contract-manifest.json")
jq -e 'all(.route_groups[]; .registration_count == (.paths | length))' "$evidence_dir/route-root-contract-manifest.json" >/dev/null \
  || fail "a route group count does not match its path list"
[[ "$route_source_count" == "$manifest_route_count" && "$manifest_route_count" == "$group_route_count" ]] \
  || fail "route count drift: source=$route_source_count manifest=$manifest_route_count groups=$group_route_count"

duplicate_routes=$(jq -r '[.route_groups[].paths[]] | group_by(.) | map(select(length > 1) | .[0]) | .[]' \
  "$evidence_dir/route-root-contract-manifest.json")
[[ -z "$duplicate_routes" ]] || fail "duplicate registered route paths: $duplicate_routes"

docs_manifest=$(mktemp)
docs_source=$(mktemp)
trap 'rm -f "$docs_manifest" "$docs_source"' EXIT
jq -r '.documentation_pages[]' "$evidence_dir/route-root-contract-manifest.json" | sort >"$docs_manifest"
(
  cd "$repo_root"
  rg --files docs/src -g '*.md' | sort
) >"$docs_source"
cmp -s "$docs_manifest" "$docs_source" || fail "documentation page inventory drifted"

jq -e '
  ([.entry_type_to_kv_tag[].entry_tag] | length) == ([.entry_type_to_kv_tag[].entry_tag] | unique | length) and
  ([.entry_type_to_kv_tag[].kv_tag] | length) == ([.entry_type_to_kv_tag[].kv_tag] | unique | length) and
  ([.kv_only_tags[].tag] | length) == ([.kv_only_tags[].tag] | unique | length) and
  ([.persistent_formats[].id] | length) == ([.persistent_formats[].id] | unique | length)
' "$evidence_dir/persisted-producer-consumer-inventory.json" >/dev/null || fail "persistent format or tag ids are duplicated"

while IFS= read -r source_path; do
  [[ -e "$repo_root/$source_path" ]] || fail "inventoried source path is missing: $source_path"
done < <(
  jq -r '
    .stable_keys_and_root_mutation.head_update_callers[],
    .stable_keys_and_root_mutation.raw_entry_writer_files[],
    .stable_keys_and_root_mutation.directory_mutator_caller_files[]
  ' "$evidence_dir/persisted-producer-consumer-inventory.json" | sort -u
)

jq -e '
  all(.fixes[];
    if .status == "guarded" then (.guards | length) > 0
    elif .status == "missing_named_guard" then (.required_guard | length) > 0
    else false
    end
  ) and
  (.open_gaps | type == "array")
' "$evidence_dir/recent-fix-ledger.json" >/dev/null || fail "recent-fix guard classification is incomplete"

if rg -n 'aeor_k_' "$evidence_dir" >/dev/null; then
  fail "secret-shaped API key found in committed evidence"
fi
if find "$evidence_dir" -type f \( -name '*.aeordb' -o -name '*.db' -o -name '*.sqlite' \) -print -quit | grep -q .; then
  fail "database artifact found in committed evidence"
fi

reference_root="$repo_root/tools/v4-reference"
fixture_root="$repo_root/aeordb-lib/spec/fixtures/v4"
[[ -f "$reference_root/Cargo.toml" ]] || fail "missing independent v4 reference Cargo manifest"
[[ -f "$reference_root/src/main.rs" ]] || fail "missing independent v4 reference implementation"
[[ -f "$fixture_root/format-contract-registry.json" ]] || fail "missing v4 format contract registry"
[[ -f "$fixture_root/format-fixture-manifest.json" ]] || fail "missing v4 format fixture manifest"

contract_registry="$fixture_root/format-contract-registry.json"
fixture_manifest="$fixture_root/format-fixture-manifest.json"
result_ledger="$fixture_root/reference-result-ledger.json"
jq -e --arg campaign "$campaign_id" '
  .schema_version == 1 and
  .campaign_id == $campaign and
  .coverage_stage == "p0b-1-seed" and
  ([.hash_algorithms[].id] | length) == ([.hash_algorithms[].id] | unique | length) and
  ([.capability_bits[].bit] | length) == 24 and
  ([.capability_bits[].bit] | unique | length) == 24 and
  (.capability_bits[] | select(.bit == 17).name) == "RootLifecycleRetirementV1" and
  (.formats | length) == 1 and
  .formats[0].id == "database-header-v4" and
  .formats[0].slot_length == 1024 and
  .formats[0].slot_count == 2 and
  .formats[0].data_offset == 2048 and
  (.formats[0].layout | length) == 36 and
  ([.formats[0].layout[] | .offset + .length] | max) == 1024 and
  any(.formats[0].layout[]; .field == "physical_instance_id" and .offset == 464 and .length == 16) and
  any(.formats[0].layout[]; .field == "slot_crc32" and .offset == 1020 and .length == 4) and
  (.formats[0].fixture_ids_32 | length) > 0 and
  (.formats[0].fixture_ids_64 | length) > 0
' "$contract_registry" >/dev/null || fail "P0b-1 format contract registry is incomplete"

jq -e --arg campaign "$campaign_id" '
  .schema_version == 1 and
  .campaign_id == $campaign and
  .stage == "p0b-1-seed" and
  .reference_tool.production_dependencies == [] and
  .reference_tool.reviewer_status == "pending-owner-review-before-production-writer" and
  .fixture_count == 10 and
  .fixture_count == (.fixtures | length) and
  ([.fixtures[].id] | length) == ([.fixtures[].id] | unique | length) and
  any(.fixtures[]; .hash_width == 32) and
  any(.fixtures[]; .hash_width == 64) and
  all(.fixtures[]; .byte_length == 2048) and
  any(.fixtures[]; .expected == "error:ambiguous_equal_sequence") and
  any(.fixtures[]; .expected == "error:unsupported_required_capability") and
  any(.fixtures[]; .expected == "error:reserved_nonzero") and
  any(.fixtures[]; .relation == "adopts:header-blake3-256-valid-ab")
' "$fixture_manifest" >/dev/null || fail "P0b-1 fixture manifest is incomplete"

diff -u \
  <(jq -r '.formats[0].fixture_ids_32[], .formats[0].fixture_ids_64[]' "$contract_registry" | sort) \
  <(jq -r '.fixtures[].id' "$fixture_manifest" | sort) >/dev/null \
  || fail "contract-registry fixture IDs differ from the fixture manifest"

while IFS=$'\t' read -r binary annotation; do
  [[ -f "$fixture_root/$binary" ]] || fail "missing fixture binary: $binary"
  [[ -f "$fixture_root/$annotation" ]] || fail "missing annotated fixture hex: $annotation"
  [[ "$(stat -c %s "$fixture_root/$binary")" == "2048" ]] || fail "fixture binary is not 2048 bytes: $binary"
done < <(jq -r '.fixtures[] | [.binary, .annotated_hex] | @tsv' "$fixture_manifest")

jq -e --arg campaign "$campaign_id" '
  .schema_version == 1 and
  .campaign_id == $campaign and
  (.results | length) == 10 and
  all(.results[]; .result == "pass" and .expected == .observed)
' "$result_ledger" >/dev/null || fail "P0b-1 reference result ledger is not green"

reference_jobs=${CARGO_BUILD_JOBS:-4}
if ((reference_jobs > 6)); then
  reference_jobs=6
fi
reference_target=${AEORDB_V4_REFERENCE_TARGET_DIR:-/tmp/codex/aeordb-v4-reference-target}
CARGO_TARGET_DIR="$reference_target" cargo run -j "$reference_jobs" --locked --quiet \
  --manifest-path "$reference_root/Cargo.toml" -- verify "$fixture_root" \
  || fail "independent v4 reference verification failed"

printf 'v4 P0 contract evidence: PASS (%s routes, %s docs, entry %s)\n' \
  "$manifest_route_count" "$(wc -l <"$docs_manifest")" "$entry_commit"
