#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
evidence_dir="$repo_root/bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor/evidence"
campaign_id="aeordb-v4-nvt-gc-2026-08-03"

fail() {
  printf 'v4 contract check failed: %s\n' "$*" >&2
  exit 1
}

sha256_stream() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 | awk '{print $1}'
    return
  fi
  fail "neither sha256sum nor shasum is available"
}

sha256_file() {
  sha256_stream < "$1"
}

canonical_text_file() {
  sed 's/\r$//' "$1"
}

sha256_canonical_text_file() {
  canonical_text_file "$1" | sha256_stream
}

file_size_bytes() {
  wc -c < "$1" | tr -d '[:space:]'
}

canonical_text_file_size_bytes() {
  canonical_text_file "$1" | wc -c | tr -d '[:space:]'
}

normalize_text_lines() {
  tr -d '\r'
}

normalize_inventory_paths() {
  normalize_text_lines | sed 's|\\|/|g'
}

for tool in awk cargo git jq rg python3 sed tr wc; do
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

entry_commit=$(jq -r '.source.entry_commit' "$evidence_dir/baseline-environment.json" | normalize_text_lines)
git -C "$repo_root" cat-file -e "$entry_commit^{commit}" 2>/dev/null || fail "entry commit $entry_commit is not in repository history"

behavior_commit=$(jq -r '.source_commit' "$evidence_dir/baseline-behavior-and-performance.json" | normalize_text_lines)
inventory_commit=$(jq -r '.source_commit' "$evidence_dir/persisted-producer-consumer-inventory.json" | normalize_text_lines)
route_commit=$(jq -r '.source_commit' "$evidence_dir/route-root-contract-manifest.json" | normalize_text_lines)
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

route_source_count=$(rg -c '\.route\s*\(' "$repo_root/aeordb-lib/src/server/mod.rs" | normalize_text_lines)
manifest_route_count=$(jq '.route_registration_count' "$evidence_dir/route-root-contract-manifest.json" | normalize_text_lines)
group_route_count=$(jq '[.route_groups[].paths | length] | add' "$evidence_dir/route-root-contract-manifest.json" | normalize_text_lines)
jq -e 'all(.route_groups[]; .registration_count == (.paths | length))' "$evidence_dir/route-root-contract-manifest.json" >/dev/null \
  || fail "a route group count does not match its path list"
[[ "$route_source_count" == "$manifest_route_count" && "$manifest_route_count" == "$group_route_count" ]] \
  || fail "route count drift: source=$route_source_count manifest=$manifest_route_count groups=$group_route_count"

duplicate_routes=$(jq -r '[.route_groups[].paths[]] | group_by(.) | map(select(length > 1) | .[0]) | .[]' \
  "$evidence_dir/route-root-contract-manifest.json" | normalize_text_lines)
[[ -z "$duplicate_routes" ]] || fail "duplicate registered route paths: $duplicate_routes"

docs_manifest=$(mktemp)
docs_source=$(mktemp)
trap 'rm -f "$docs_manifest" "$docs_source"' EXIT
jq -r '.documentation_pages[]' "$evidence_dir/route-root-contract-manifest.json" | normalize_inventory_paths | sort >"$docs_manifest"
(
  cd "$repo_root"
  rg --files docs/src -g '*.md' | normalize_inventory_paths | sort
) >"$docs_source"
cmp -s "$docs_manifest" "$docs_source" || fail "documentation page inventory drifted"

migration_doc="$repo_root/docs/src/operations/migration.md"
[[ -f "$migration_doc" ]] || fail "missing v3-to-v4 migration operator documentation"
rg -Fq '`aeordb migrate-v4` builds and verifies a separate shadow only.' "$migration_doc" \
  || fail "migration documentation does not expose the versioned shadow-only command"
rg -Fq 'public `aeordb cutover`, HTTP migration route, service activation' "$migration_doc" \
  || fail "migration documentation does not preserve the no-public-activation boundary"
rg -Fq 'Operator acceptance and first v4 write' "$migration_doc" \
  || fail "migration documentation does not separate acceptance from the first v4 write"
rg -Fq 'Destructive v4 GC' "$migration_doc" \
  || fail "migration documentation does not preserve the destructive-GC authorization boundary"

hot_dir_contract_paths=(
  "$repo_root/aeordb-cli/src/main.rs"
  "$repo_root/aeordb-cli/src/config.rs"
  "$repo_root/aeordb-cli/src/commands/start.rs"
  "$repo_root/aeordb-lib/src/server/mod.rs"
  "$repo_root/aeordb-lib/src/engine/storage_engine.rs"
  "$repo_root/docs/src/SKILL.md"
  "$repo_root/docs/src/cli/commands.md"
  "$repo_root/docs/src/getting-started/configuration.md"
  "$repo_root/deploy/systemd/README.md"
  "$repo_root/aeordb.example.toml"
)
if rg -n \
  'Directory for write-ahead hot files|hot directory for crash recovery|hot directory for crash-recovery|Replays any existing hot files|DB file and any hot-dir|database file, hot files' \
  "${hot_dir_contract_paths[@]}" >/dev/null; then
  fail "an active CLI, API, or operator surface still advertises the retired hot-directory implementation"
fi
for path in \
  "$repo_root/aeordb-cli/src/main.rs" \
  "$repo_root/aeordb-cli/src/config.rs" \
  "$repo_root/docs/src/cli/commands.md" \
  "$repo_root/docs/src/getting-started/configuration.md"; do
  rg -iq 'legacy compatibility' "$path" \
    || fail "hot-dir compatibility status is absent from ${path#"$repo_root/"}"
done
debt_policy="$evidence_dir/v4-debt-policy.json"
jq -e '
  any(.entries[];
    .id == "legacy-hot-dir-option" and
    .classification == "timed_compatibility_shim" and
    .maximum_matches > 0 and
    (.owner | length) > 0 and
    (.removal_gate | length) > 0)
' "$debt_policy" >/dev/null \
  || fail "the retained hot-dir API is absent from the reviewed compatibility-debt policy"

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
  ' "$evidence_dir/persisted-producer-consumer-inventory.json" | normalize_inventory_paths | sort -u
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
generated_contract="$repo_root/aeordb-lib/src/engine/v4/contract_generated.rs"
architecture_registry="$fixture_root/architecture-contract-registry.json"
[[ -f "$reference_root/Cargo.toml" ]] || fail "missing independent v4 reference Cargo manifest"
[[ -f "$reference_root/src/main.rs" ]] || fail "missing independent v4 reference implementation"
[[ -f "$fixture_root/format-contract-registry.json" ]] || fail "missing v4 format contract registry"
[[ -f "$fixture_root/format-fixture-manifest.json" ]] || fail "missing v4 format fixture manifest"
[[ -f "$architecture_registry" ]] || fail "missing v4 architecture contract registry"
[[ -f "$generated_contract" ]] || fail "missing generated v4 Rust contract constants"

contract_registry="$fixture_root/format-contract-registry.json"
fixture_manifest="$fixture_root/format-fixture-manifest.json"
result_ledger="$fixture_root/reference-result-ledger.json"
expected_format_count=$(jq -er '.p0b_progress.fixture_family_count | numbers' "$contract_registry" | normalize_text_lines) \
  || fail "P0b progress lacks a numeric fixture-family count"
expected_fixture_count=$(jq -er '.p0b_progress.fixture_count | numbers' "$contract_registry" | normalize_text_lines) \
  || fail "P0b progress lacks a numeric fixture count"
jq -e --arg campaign "$campaign_id" --argjson format_count "$expected_format_count" --argjson fixture_count "$expected_fixture_count" '
  .schema_version == 1 and
  .campaign_id == $campaign and
  .coverage_stage == "p0b-2-system-family" and
  ([.hash_algorithms[].id] | length) == ([.hash_algorithms[].id] | unique | length) and
  ([.capability_bits[].bit] | length) == 24 and
  ([.capability_bits[].bit] | unique | length) == 24 and
  (.capability_bits[] | select(.bit == 17).name) == "RootLifecycleRetirementV1" and
  (.formats | length) == $format_count and
  .p0b_progress.fixture_family_count == $format_count and
  .p0b_progress.fixture_count == $fixture_count and
  ([.formats[].id] | length) == ([.formats[].id] | unique | length) and
  all(.formats[];
    (.identity | length) > 0 and
    (.body_formula | length) > 0 and
    .hard_cap > 0 and
    (.checksum | length) > 0 and
    (.canonical_order | length) > 0 and
    (.reserve_zero_ranges | length) > 0 and
    (.malformed_behavior | length) > 0 and
    (.trailing_behavior | length) > 0 and
    (.bounded_decode | length) > 0 and
    (.producer_owner | length) > 0 and
    (.consumer_owners | length) > 0 and
    (.capability | length) > 0 and
    (.typed_hash_roles | length) > 0 and
    (.fixture_ids_32 | length) > 0 and
    (.fixture_ids_64 | length) > 0) and
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
' "$contract_registry" >/dev/null || fail "P0b-2 core format contract registry is incomplete"

jq -e '
  (.persistent_registries.entry_type_v1 | length) == 10 and
  ([.persistent_registries.entry_type_v1[].id] | unique | length) == 10 and
  ([.persistent_registries.entry_type_v1[].name] | unique | length) == 10 and
  (.persistent_registries.entry_type_v1 | map([.id, .kv_tag])) ==
    [[1,0],[2,1],[3,2],[4,3],[5,4],[6,5],[7,7],[8,9],[9,10],[10,11]] and
  (.persistent_registries.kv_tag_v1 | length) == 12 and
  ([.persistent_registries.kv_tag_v1[].id] | sort) == [0,1,2,3,4,5,6,7,8,9,10,11] and
  ([.persistent_registries.kv_tag_v1[].name] | unique | length) == 12 and
  (.persistent_registries.shared_enums | keys | sort) == [
    "audit_pin_reason_v1", "durability_operation_v1", "gc_audit_event_kind_v1",
    "gc_error_class_v1", "gc_outcome_v1", "gc_run_kind_v1", "mutation_operation_v1",
    "os_error_class_v1", "repair_corruption_class_v1", "retry_class_v1",
    "root_retirement_reason_v1", "stable_reason_v1", "task_kind_v1"
  ] and
  all(.persistent_registries.shared_enums[];
    length > 0 and
    ([.[].id] | length) == ([.[].id] | unique | length) and
    ([.[].name] | length) == ([.[].name] | unique | length) and
    all(.[]; (.name | length) > 0)) and
  (.malformed_input_classes | length) == 16 and
  (.malformed_input_classes | unique | length) == 16 and
  (.registry_invariants.mutation_dimensions | sort) ==
    ["bit","checksum","enum","field","identity","length","ordering","reserve","trailing"] and
  .registry_invariants.future_entry_types == "0x0b through 0xff reserved" and
  .registry_invariants.future_kv_tags == "0x0c through 0x0f reserved"
' "$contract_registry" >/dev/null || fail "P0b-2 persistent enum/capability registry closure is incomplete"

registry_report="$evidence_dir/p0b-contract-registry-report.json"
[[ -f "$registry_report" ]] || fail "missing P0b collision/ID/capability registry report"
jq -e --arg source_sha "$(sha256_canonical_text_file "$contract_registry")" \
  --arg fixture_sha "$(sha256_canonical_text_file "$fixture_manifest")" \
  --argjson format_count "$expected_format_count" --argjson fixture_count "$expected_fixture_count" '
  .schema_version == 1 and .campaign_id == "aeordb-v4-nvt-gc-2026-08-03" and
  .source_sha256 == $source_sha and .fixture_manifest_sha256 == $fixture_sha and
  .counts.formats == $format_count and .counts.fixtures == $fixture_count and
  .counts.capability_bits == 24 and .counts.entry_types == 10 and .counts.kv_tags == 12 and
  .counts.shared_enum_scopes == 13 and .counts.shared_enum_values == 134 and
  .counts.system_families == 46 and .counts.system_family_descriptors == 63 and
  .counts.malformed_input_classes == 16 and
  all(.collision_results[]; . == 0) and
  all(.coverage[]; . == true or type == "string") and
  (.malformed_proof_dimensions | length) == 9 and
  .reference_tool.production_dependencies == [] and
  .reference_tool.tests_passed >= 142
' "$registry_report" >/dev/null || fail "P0b collision/ID/capability registry report is stale or incomplete"

p0c_report="$evidence_dir/p0c-machine-contract-report.json"
[[ -f "$p0c_report" ]] || fail "missing P0c machine contract report"
jq -e --argjson fixture_count "$expected_fixture_count" --argjson route_count "$manifest_route_count" \
  --argjson docs_count "$(wc -l <"$docs_manifest")" \
  --arg architecture_sha "$(sha256_canonical_text_file "$architecture_registry")" \
  --arg generated_sha "$(sha256_canonical_text_file "$generated_contract")" \
  --argjson generated_bytes "$(canonical_text_file_size_bytes "$generated_contract")" '
  .schema_version == 1 and .campaign_id == "aeordb-v4-nvt-gc-2026-08-03" and
  .landing_unit == "P0c" and
  .architecture_registry_sha256 == $architecture_sha and
  .generated_rust_sha256 == $generated_sha and
  .generated_rust_bytes == $generated_bytes and
  .counts == {
    "route_classes":7,"configuration_properties":41,"dynamic_records":8,
    "hard_transitions":12,"cleanup_result_classes":4,"semantic_bundles":37
  } and
  .proof.reference_tests_passed >= 144 and
  .proof.production_tests_passed >= 3 and
  .proof.strict_reference_clippy == "pass" and
  .proof.rustfmt_check == "pass" and
  .proof.fixture_cases_verified == $fixture_count and
  .proof.route_registrations_inventoried == $route_count and
  .proof.documentation_pages_inventoried == $docs_count and
  .proof.v4_writers_activated == false and
  (.failure_cases | length) >= 7
' "$p0c_report" >/dev/null || fail "P0c machine contract report is stale or incomplete"

jq -e --arg campaign "$campaign_id" --argjson fixture_count "$expected_fixture_count" '
  .schema_version == 1 and
  .campaign_id == $campaign and
  .stage == "p6-3b-tombstone-only-manifests" and
  .reference_tool.production_dependencies == [] and
  .reference_tool.reviewer_status == "pending-owner-review-before-production-writer" and
  .fixture_count == $fixture_count and
  .fixture_count == (.fixtures | length) and
  ([.fixtures[].id] | length) == ([.fixtures[].id] | unique | length) and
  any(.fixtures[]; .hash_width == 32) and
  any(.fixtures[]; .hash_width == 64) and
  all(.fixtures[]; .byte_length > 0) and
  any(.fixtures[]; .expected == "error:ambiguous_equal_sequence") and
  any(.fixtures[]; .expected == "error:unsupported_required_capability") and
  any(.fixtures[]; .expected == "error:reserved_nonzero") and
  any(.fixtures[]; .relation == "adopts:header-blake3-256-valid-ab") and
  all(.fixtures[]; has("format_id") and has("canonical_key"))
' "$fixture_manifest" >/dev/null || fail "P0b-2 core fixture manifest is incomplete"

diff -u \
  <(jq -r '.formats[] | .fixture_ids_32[], .fixture_ids_64[]' "$contract_registry" | normalize_text_lines | sort) \
  <(jq -r '.fixtures[].id' "$fixture_manifest" | normalize_text_lines | sort) >/dev/null \
  || fail "contract-registry fixture IDs differ from the fixture manifest"

while IFS=$'\t' read -r binary annotation byte_length; do
  [[ -f "$fixture_root/$binary" ]] || fail "missing fixture binary: $binary"
  [[ -f "$fixture_root/$annotation" ]] || fail "missing annotated fixture hex: $annotation"
  [[ "$(file_size_bytes "$fixture_root/$binary")" == "$byte_length" ]] || fail "fixture binary length differs from manifest: $binary"
done < <(jq -r '.fixtures[] | [.binary, .annotated_hex, .byte_length] | @tsv' "$fixture_manifest" | normalize_inventory_paths)

jq -e --arg campaign "$campaign_id" --argjson fixture_count "$expected_fixture_count" '
  .schema_version == 1 and
  .campaign_id == $campaign and
  (.results | length) == $fixture_count and
  all(.results[]; .result == "pass" and .expected == .observed)
' "$result_ledger" >/dev/null || fail "P0b-2 core reference result ledger is not green"

required_p0b2_core_formats=(
  whole-entity-v1
  directory-index-v1
  semantic-object-v1
)
for format_id in "${required_p0b2_core_formats[@]}"; do
  jq -e --arg format_id "$format_id" 'any(.formats[]; .id == $format_id)' \
    "$contract_registry" >/dev/null \
    || fail "P0b-2 core format is absent from the contract registry: $format_id"
  jq -e --arg format_id "$format_id" \
    'any(.fixtures[]; .format_id == $format_id and .hash_width == 32) and
     any(.fixtures[]; .format_id == $format_id and .hash_width == 64)' \
    "$fixture_manifest" >/dev/null \
    || fail "P0b-2 core format lacks both hash-width fixtures: $format_id"
done

jq -e 'any(.formats[]; .id == "index-artifact-v1")' "$contract_registry" >/dev/null \
  || fail "P0b-2 index format is absent from the contract registry: index-artifact-v1"
jq -e '
  any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 32) and
  any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 64)
' "$fixture_manifest" >/dev/null \
  || fail "P0b-2 index format lacks both hash-width fixtures"
required_p0b2_manifest_results=(
  'index:manifest:scope-catalog:'
  'index:manifest:value-store:'
  'index:manifest:field-index:'
  'index:manifest:field-nvt:'
)
for result_prefix in "${required_p0b2_manifest_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 immutable manifest lacks both hash-width fixtures: $result_prefix"
done

required_p0b2_page_results=(
  'index:directory:scope-ordinal:'
  'index:directory:scope-reverse:'
  'index:directory:value:'
  'index:directory:value-document-state:'
  'index:directory:posting:'
  'index:directory:index-document-state:'
  'index:page:posting:'
  'index:page:value:'
  'index:page:scope-catalog:ordinal:'
  'index:page:scope-catalog:reverse:'
  'index:page:document-state:value-store:'
  'index:page:document-state:index:'
)
for result_prefix in "${required_p0b2_page_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 ordered artifact lacks both hash-width fixtures: $result_prefix"
done

required_p0b2_nvt_results=(
  'index:nvt-tile:'
  'index:directory:nvt-tile:'
)
for result_prefix in "${required_p0b2_nvt_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 NVT artifact lacks both hash-width fixtures: $result_prefix"
done

required_p0b2_task_results=(
  'index:journal:task:'
  'index:journal:system:'
  'index:checkpoint:embedded:'
  'index:checkpoint:external:'
)
for result_prefix in "${required_p0b2_task_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "index-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 index task artifact lacks both hash-width fixtures: $result_prefix"
done

required_p0b2_position_results=(
  'position:directory-listing:'
  'position:query:'
  'position:global-search:'
  'position:aggregate-groups:'
  'position:maximum:'
)
for result_prefix in "${required_p0b2_position_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "logical-position-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "logical-position-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 APOS lacks both hash-width fixtures: $result_prefix"
done

jq -e 'any(.formats[]; .id == "gc-artifact-v1")' "$contract_registry" >/dev/null \
  || fail "P0b-2 GC format is absent from the contract registry: gc-artifact-v1"
jq -e '
  .formats[] | select(.id == "gc-artifact-v1") |
  (.kind_registry | length) == 34 and
  .kind_registry["0x0006"] == "RootLifecycleActiveControl" and
  .kind_registry["0x0017"] == "RootLifecycleManifest" and
  .kind_registry["0x0028"] == "RootCandidatePage" and
  .kind_registry["0x0037"] == "RootRetirementCommit" and
  .kind_registry["0x0038"] == "VoidClaimSettlementReceipt" and
  .kind_registry["0x0039"] == "RootObjectReclaimProof" and
  (.active_control.target_kinds | length) == 6 and
  .active_control.target_kinds["0x0006"] == "0x0017" and
  ((.frozen_body_kinds + .pending_body_fixture_kinds) | length) == 34 and
  ((.frozen_body_kinds + .pending_body_fixture_kinds) | unique | length) == 34 and
  .quarantine_state.manifest_body_formula == "100 + 6H + D*H" and
  .root_lifecycle_state.manifest_body_formula == "108 + 3H" and
  .root_lifecycle_state.expiry_manifest_body_formula == "124 + H" and
  .root_lifecycle_state.expiry_row_length == "40 + 3H" and
  .root_lifecycle_state.retirement_body_formula == "72 + 4H" and
  .root_lifecycle_state.reclaim_proof_body_formula == "40 + 6H" and
  .physical_inventory_state.manifest_body_formula == "132 + 2H" and
  .physical_inventory_state.retirement_record_length == "72 + 4H" and
  .bounded_mark_state.checkpoint_body_formula == "236 + 4H + P" and
  .bounded_mark_state.checkpoint_body_cap == 262144 and
  .bounded_mark_state.journal_payload_length == "36 + 6H" and
  .bounded_mark_state.journal_framed_record_length == "40 + 6H"
' "$contract_registry" >/dev/null \
  || fail "P0b-2 corrected GC registry/state contract is incomplete"
required_p0b2_gc_control_results=(
  'gc:control:quarantine:'
  'gc:control:mark-run:'
  'gc:control:physical-inventory:'
  'gc:control:audit-catalog:'
  'gc:control:void-catalog:'
  'gc:control:root-lifecycle:'
)
for result_prefix in "${required_p0b2_gc_control_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 GC active control lacks both hash-width fixtures: $result_prefix"
done
required_p0b2_gc_state_results=(
  'gc:manifest:quarantine:empty:'
  'gc:manifest:quarantine:populated:'
  'gc:page:candidate:'
  'gc:delta:candidate:'
  'gc:manifest:root-expiry:empty:'
  'gc:manifest:root-expiry:populated:'
  'gc:page:root-expiry:'
  'gc:journal:retirement:'
  'gc:manifest:physical-inventory:empty:'
  'gc:manifest:physical-inventory:populated:'
  'gc:page:physical-inventory:'
  'gc:directory:candidates:'
  'gc:directory:root-expiry:'
  'gc:directory:physical-inventory:'
  'gc:manifest:root-lifecycle:empty:'
  'gc:manifest:root-lifecycle:populated:'
  'gc:page:root-candidate:'
  'gc:directory:root-candidates:'
  'gc:commit:root-retirement:'
  'gc:proof:root-object-reclaim:'
)
for result_prefix in "${required_p0b2_gc_state_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 GC state artifact lacks both hash-width fixtures: $result_prefix"
done

required_p0b2_mark_formats=(
  gc-mark-workspace-manifest-v1
  gc-mark-workspace-object-v1
)
for format_id in "${required_p0b2_mark_formats[@]}"; do
  jq -e --arg format_id "$format_id" 'any(.formats[]; .id == $format_id)' \
    "$contract_registry" >/dev/null \
    || fail "P0b-2 mark workspace format is absent from the contract registry: $format_id"
  jq -e --arg format_id "$format_id" \
    'any(.fixtures[]; .format_id == $format_id and .hash_width == 32) and
     any(.fixtures[]; .format_id == $format_id and .hash_width == 64)' \
    "$fixture_manifest" >/dev/null \
    || fail "P0b-2 mark workspace format lacks both hash-width fixtures: $format_id"
done
jq -e '
  (.formats[] | select(.id == "gc-mark-workspace-manifest-v1") |
    .magic_ascii == "AGCW" and .version == 1 and
    .fixed_length_formula == "120 + 2H before descriptors; complete length is 124 + 2H + sum(68 + name_length)" and
    .descriptor_fixed_length == 68 and .object_count_cap == 65535 and .hard_cap == 8388608) and
  (.formats[] | select(.id == "gc-mark-workspace-object-v1") |
    .magic_ascii == "AGWO" and .version == 1 and .fixed_header_length == 80 and
    .bitmap_body_formula == "32 + ceil(logical_bit_count/8)" and
    .record_cap == 1048576 and .hard_cap == 67108864 and
    (.kind_registry | length) == 6)
' "$contract_registry" >/dev/null \
  || fail "P0b-2 bounded mark workspace formulas are incomplete"

required_p0b2_mark_gc_results=(
  'gc:checkpoint:mark-run:'
  'gc:journal:mark-mutation:'
)
for result_prefix in "${required_p0b2_mark_gc_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 bounded-mark GC artifact lacks both hash-width fixtures: $result_prefix"
done

required_p0b2_workspace_results=(
  'gc:workspace-manifest:'
  'gc:workspace-object:bitmap:'
  'gc:workspace-object:frontier:'
  'gc:workspace-object:path-visit:'
  'gc:workspace-object:mutation:'
  'gc:workspace-object:candidate:'
  'gc:workspace-object:diagnostic:'
)
for result_prefix in "${required_p0b2_workspace_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; (.format_id | startswith("gc-mark-workspace-")) and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; (.format_id | startswith("gc-mark-workspace-")) and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 mark workspace artifact lacks both hash-width fixtures: $result_prefix"
done

jq -e '
  .formats[] | select(.id == "gc-artifact-v1") |
  .sweep_void_state.sweep_proposal_body_formula == "32 + 2H + N*(24 + 2H)" and
  .sweep_void_state.void_manifest_body_formula == "92 + 2H" and
  .sweep_void_state.void_extent_row_length == "32 + 3H" and
  .sweep_void_state.void_claim_fixed_length == "56 + H" and
  .sweep_void_state.settlement_body_length == "40 + 3H"
' "$contract_registry" >/dev/null \
  || fail "P0b-2 corrected sweep/Void formulas are incomplete"

required_p0b2_sweep_void_results=(
  'gc:proposal:sweep:'
  'gc:receipt:sweep-commit:'
  'gc:receipt:sweep-recovered:'
  'gc:manifest:void-catalog:empty:'
  'gc:manifest:void-catalog:populated:'
  'gc:page:void-free-extents:'
  'gc:directory:void-free-extents:'
  'gc:claim:void:'
  'gc:directory:void-claims:'
  'gc:receipt:void-claim-settlement:'
)
for result_prefix in "${required_p0b2_sweep_void_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 sweep/Void artifact lacks both hash-width fixtures: $result_prefix"
done

jq -e '
  .formats[] | select(.id == "gc-artifact-v1") |
  .audit_state.catalog_fixed_body_formula == "148 + 2H + P*H" and
  .audit_state.detail_record_fixed_length == "52 + H" and
  .audit_state.summary_record_length == "76 + H" and
  .audit_state.corrupt_evidence_fixed_body_formula == "68 + 3H" and
  .audit_state.audit_pin_fixed_body_formula == "32 + H" and
  .directory_roles["6"] == "audit_detail: occurred_at_ms then event_id order" and
  .directory_roles["7"] == "audit_summary: completed_at_ms then run_id order" and
  (.pending_body_fixture_kinds | length) == 0
' "$contract_registry" >/dev/null \
  || fail "P0b-2 GC audit/evidence formulas or registry closure are incomplete"

required_p0b2_gc_audit_results=(
  'gc:manifest:audit-catalog:empty:'
  'gc:manifest:audit-catalog:populated:'
  'gc:page:audit-detail:'
  'gc:directory:audit-detail:'
  'gc:page:audit-summary:'
  'gc:directory:audit-summary:'
  'gc:summary:run:'
  'gc:evidence:corrupt:'
  'gc:pin:audit:'
)
for result_prefix in "${required_p0b2_gc_audit_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; .format_id == "gc-artifact-v1" and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 GC audit/evidence artifact lacks both hash-width fixtures: $result_prefix"
done

jq -e '
  .formats[] | select(.id == "system-control-v1") |
  .magic_registry == {
    "0x0001":"AIRG", "0x0002":"AIOP", "0x0003":"AIDG",
    "0x0010":"ALLG", "0x0011":"ALDG", "0x0012":"ARLG", "0x0013":"ARDG",
    "0x0020":"ARTK", "0x0021":"APWL",
    "0x0030":"AMLE", "0x0031":"AMPR", "0x0032":"ALRM", "0x0033":"ALRP",
    "0x0040":"ATPN", "0x0041":"ASMJ", "0x0042":"ARTX", "0x0043":"ARAC",
    "0x0050":"ADLT", "0x0051":"ASPC", "0x0052":"ACUT"
  } and
  .version == 1 and .header_length == 32 and .identity_length_cap == 4096 and
  .physical_representation.root == "/.aeordb-system/controls/v1/" and
  .physical_representation.mutable_slots == ["a.ctrl", "b.ctrl"] and
  .physical_representation.immutable_slot == "i.ctrl" and
  .physical_representation.content_type == "application/vnd.aeordb.system-control" and
  (.body_contracts | length) == 20 and
  (.pending_body_fixture_kinds | length) == 0
' "$contract_registry" >/dev/null \
  || fail "P0b-2 SystemControl registry/framing is incomplete"

required_p0b2_system_control_results=(
  'control:index-registry:'
  'control:index-operation:'
  'control:index-degraded:'
  'control:lifecycle-lkg:'
  'control:lifecycle-diagnostics:'
  'control:runtime-lkg:'
  'control:runtime-diagnostics:'
  'control:repair-ticket:'
  'control:path-write-latch:'
  'control:migration-lease:'
  'control:migration-progress:'
  'control:legacy-root-map:'
  'control:legacy-root-map-page:'
  'control:task-pin:'
  'control:semantic-mutation-segment:'
  'control:root-publication-prepare:'
  'control:root-admission-commit:'
  'control:durability-latch:'
  'control:emergency-spill-catalog:'
  'control:side-by-side-cutover:'
  'cutover:external-journal:'
)
for result_prefix in "${required_p0b2_system_control_results[@]}"; do
  jq -e --arg result_prefix "$result_prefix" '
    any(.fixtures[]; (.format_id == "system-control-v1" or .format_id == "cutover-journal-v1") and .hash_width == 32 and (.expected | startswith($result_prefix))) and
    any(.fixtures[]; (.format_id == "system-control-v1" or .format_id == "cutover-journal-v1") and .hash_width == 64 and (.expected | startswith($result_prefix)))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 system control/cutover artifact lacks both hash-width fixtures: $result_prefix"
done

jq -e '
  .formats[] | select(.id == "migration-capture-v1") |
  .magic_ascii == "AMCM" and .version == 1 and
  .fixed_length_formula == "276 + 7H; 500 bytes at H=32 and 724 bytes at H=64" and
  .hard_cap == 724 and
  .capability == "SideBySideMigrationV1 bit 21" and
  (.layout | length) == 16 and
  (.reserve_zero_ranges | length) == 3 and
  (.typed_hash_roles | length) == 5
' "$contract_registry" >/dev/null \
  || fail "P3c-2b1 migration capture contract registry is incomplete"
jq -e '
  any(.fixtures[]; .format_id == "migration-capture-v1" and .hash_width == 32 and (.expected | startswith("migration:capture:"))) and
  any(.fixtures[]; .format_id == "migration-capture-v1" and .hash_width == 64 and (.expected | startswith("migration:capture:")))
' "$fixture_manifest" >/dev/null \
  || fail "P3c-2b1 migration capture format lacks both hash-width fixtures"

system_family_binary="$repo_root/aeordb-lib/spec/fixtures/system-family-registry-v1.bin"
system_family_manifest="$repo_root/aeordb-lib/spec/fixtures/system-family-registry-v1.manifest.json"
[[ -f "$system_family_binary" ]] || fail "missing canonical SystemFamily registry binary"
[[ -f "$system_family_manifest" ]] || fail "missing canonical SystemFamily registry manifest"
jq -e '
  .schema_version == 1 and .registry_schema_version == 1 and
  .registry_magic == "ASFR" and .descriptor_count > 46 and .source_row_count == 46 and
  (.family_ids | length) == 46 and (.family_ids | unique | length) == 46 and
  (.family_ids | index("0x0019")) != null and
  (.family_ids | index("0x001a")) != null and
  (.descriptor_keys | length) == .descriptor_count and
  (.descriptor_keys | unique | length) == .descriptor_count and
  (.fingerprints.blake3_256 | test("^[0-9a-f]{64}$")) and
  (.fingerprints.sha512 | test("^[0-9a-f]{128}$")) and
  (.operational_control_tags | length) == 11 and
  (.external_workspace_kinds | length) == 4
' "$system_family_manifest" >/dev/null \
  || fail "P0b-2 SystemFamily manifest is incomplete"
jq -e '
  .formats[] | select(.id == "system-family-registry-v1") |
  .magic_ascii == "ASFR" and .version == 1 and .header_length == 32 and
  .descriptor_fixed_length == 32 and .family_count == 46 and
  .index_policy_registry == {"0":"not_applicable","1":"include_under_ordinary_scope","2":"exclude_from_all_indexes","3":"canonical_projection_only"} and
  .unknown_protected_family_id == "0xfffe" and
  .ordinary_user_data_index_policy == 1
' "$contract_registry" >/dev/null \
  || fail "P0b-2 SystemFamily contract registry is incomplete"
for hash_width in 32 64; do
  jq -e --argjson hash_width "$hash_width" '
    any(.fixtures[]; .format_id == "system-family-registry-v1" and .hash_width == $hash_width and (.expected | startswith("system-family:registry:")))
  ' "$fixture_manifest" >/dev/null \
    || fail "P0b-2 SystemFamily registry lacks hash-width fixture: $hash_width"
done

required_p0b2_definition_formats=(
  canonical-config-value-v1
  converter-definition-v1
  dependency-table-v1
  field-index-definition-v1
  invocation-policy-v1
  parser-resolution-plan-v1
  scope-definition-v1
  source-selector-v1
  value-store-definition-v1
)
for format_id in "${required_p0b2_definition_formats[@]}"; do
  jq -e --arg format_id "$format_id" 'any(.formats[]; .id == $format_id)' \
    "$contract_registry" >/dev/null \
    || fail "P0b-2 definition format is absent from the contract registry: $format_id"
  jq -e --arg format_id "$format_id" \
    'any(.fixtures[]; .format_id == $format_id and .hash_width == 32) and
     any(.fixtures[]; .format_id == $format_id and .hash_width == 64)' \
    "$fixture_manifest" >/dev/null \
    || fail "P0b-2 definition format lacks both hash-width fixtures: $format_id"
done

semantics_root="$repo_root/aeordb-lib/spec/semantics/v1"
semantics_registry="$semantics_root/fingerprint-registry.json"
[[ -f "$semantics_registry" ]] || fail "missing built-in semantics fingerprint registry"
jq -e '
  .schema_version == 1 and
  .domain == "aeordb.builtin-semantics.v1\u0000" and
  .file_order == ["SPEC.md", "invalid.bin", "properties.json", "vectors.bin"] and
  (.bundles | length) == 37 and
  ([.bundles[] | [.kind, .id, .corrected]] | length) == ([.bundles[] | [.kind, .id, .corrected]] | unique | length) and
  ([.bundles[].name] | length) == ([.bundles[].name] | unique | length) and
  all(.bundles[]; (.fingerprint_blake3 | test("^[0-9a-f]{64}$")))
' "$semantics_registry" >/dev/null || fail "built-in semantics fingerprint registry is incomplete"
while IFS=$'\t' read -r kind name; do
  case "$kind" in
    converter) bundle_family=converters ;;
    strategy) bundle_family=strategies ;;
    *) fail "unknown semantic bundle kind: $kind" ;;
  esac
  bundle_dir="$semantics_root/$bundle_family/$name"
  for bundle_file in SPEC.md invalid.bin properties.json vectors.bin; do
    [[ -s "$bundle_dir/$bundle_file" ]] || fail "missing semantic bundle file: ${kind}s/$name/$bundle_file"
  done
done < <(jq -r '.bundles[] | [.kind, .name] | @tsv' "$semantics_registry" | normalize_text_lines)

reference_jobs=${CARGO_BUILD_JOBS:-4}
if ((reference_jobs > 6)); then
  reference_jobs=6
fi
reference_target=${AEORDB_V4_REFERENCE_TARGET_DIR:-${CARGO_TARGET_DIR:-$repo_root/target}/v4-reference}
CARGO_TARGET_DIR="$reference_target" cargo run -j "$reference_jobs" --locked --quiet \
  --manifest-path "$reference_root/Cargo.toml" -- verify "$fixture_root" \
  || fail "independent v4 reference verification failed"
CARGO_TARGET_DIR="$reference_target" cargo run -j "$reference_jobs" --locked --quiet \
  --manifest-path "$reference_root/Cargo.toml" -- check-contracts \
  "$contract_registry" "$system_family_manifest" "$architecture_registry" "$generated_contract" \
  || fail "generated v4 Rust contract constants are stale"

"$repo_root/scripts/plan/check-v4-debt.sh" \
  || fail "reviewed v4 debt policy failed"

printf 'v4 P0 contract evidence: PASS (%s routes, %s docs, entry %s)\n' \
  "$manifest_route_count" "$(wc -l <"$docs_manifest")" "$entry_commit"
