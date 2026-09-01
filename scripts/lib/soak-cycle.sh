#!/usr/bin/env bash

wait_for_worker_window() {
  local target_pid="${1:-}"
  local window_seconds="${2:-}"
  local worker_label="${3:-worker}"
  local deadline
  local current_time
  local remaining_seconds
  local worker_status

  if ! [[ "$target_pid" =~ ^[1-9][0-9]*$ ]]; then
    printf '%s requires a positive numeric process ID, got %q\n' "$worker_label" "$target_pid" >&2
    return 2
  fi
  if ! [[ "$window_seconds" =~ ^[1-9][0-9]*$ ]]; then
    printf '%s requires a positive scheduled window, got %q\n' "$worker_label" "$window_seconds" >&2
    return 2
  fi

  deadline=$(( $(date +%s) + window_seconds ))
  while true; do
    if ! kill -0 "$target_pid" 2>/dev/null; then
      if wait "$target_pid"; then
        worker_status=0
      else
        worker_status=$?
      fi
      printf '%s exited before its scheduled window completed (status=%s)\n' "$worker_label" "$worker_status" >&2
      return 1
    fi

    current_time=$(date +%s)
    if [ "$current_time" -ge "$deadline" ]; then
      return 0
    fi
    remaining_seconds=$((deadline - current_time))
    if [ "$remaining_seconds" -gt 1 ]; then
      sleep 1
    else
      sleep "$remaining_seconds"
    fi
  done
}
