#!/usr/bin/env bash

# Shared checked-replacement policy for install-local.sh and remote deploys.
# Callers retain responsibility for quiescing the database before invoking an
# incompatible downgrade check. The CLI independently enforces that lock.

AEORDB_TRANSITION_RECOVERY_CAPABILITY="aeordb.v3-transition-recovery.v1"

aeordb_probe_transition_capability() {
  local binary="$1"
  local timeout_seconds="${AEORDB_DEPLOYMENT_PROBE_TIMEOUT_SECONDS:-15}"
  if [ ! -x "$binary" ]; then
    return 3
  fi

  local status
  if timeout "${timeout_seconds}s" "$binary" deployment-capabilities --require "$AEORDB_TRANSITION_RECOVERY_CAPABILITY" >/dev/null 2>&1; then
    status=0
  else
    status=$?
  fi
  case "$status" in
    0) return 0 ;;
    # Clap returns 2 for a command unknown to a pre-P2 AeorDB binary. The new
    # command returns 3 for a known but unsupported capability.
    2|3) return 3 ;;
    124|137)
      echo "error: timed out probing deployment capabilities from $binary" >&2
      return 1
      ;;
    *)
      echo "error: deployment capability probe failed for $binary (exit $status)" >&2
      return 1
      ;;
  esac
}

aeordb_checked_replacement() {
  local installed_binary="$1"
  local candidate_binary="$2"
  local database="$3"
  local timeout_seconds="${AEORDB_DEPLOYMENT_CHECK_TIMEOUT_SECONDS:-120}"
  local candidate_capability=""
  local inspector=""

  local candidate_probe
  if aeordb_probe_transition_capability "$candidate_binary"; then
    candidate_probe=0
  else
    candidate_probe=$?
  fi
  case "$candidate_probe" in
    0)
      candidate_capability="$AEORDB_TRANSITION_RECOVERY_CAPABILITY"
      inspector="$candidate_binary"
      ;;
    3)
      local installed_probe
      if aeordb_probe_transition_capability "$installed_binary"; then
        installed_probe=0
      else
        installed_probe=$?
      fi
      if [ "$installed_probe" -ne 0 ]; then
        echo "error: candidate does not understand transition recovery and no compatible installed inspector can prove downgrade safety" >&2
        return 1
      fi
      inspector="$installed_binary"
      ;;
    *) return 1 ;;
  esac

  local command=("$inspector" deployment-check --database "$database" --json)
  if [ -n "$candidate_capability" ]; then
    command+=(--candidate-capability "$candidate_capability")
  fi

  local check_status
  if timeout "${timeout_seconds}s" "${command[@]}"; then
    check_status=0
  else
    check_status=$?
  fi
  case "$check_status" in
    0) return 0 ;;
    3)
      echo "error: AeorDB deployment safety check refused candidate replacement" >&2
      return 3
      ;;
    124|137)
      echo "error: AeorDB deployment safety check timed out for $database" >&2
      return 1
      ;;
    *)
      echo "error: AeorDB deployment safety inspection failed for $database (exit $check_status)" >&2
      return 1
      ;;
  esac
}
