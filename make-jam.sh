#!/usr/bin/env bash
set -euo pipefail

# Default paths (override with env vars if needed)
HTML_ROOT="${HTML_ROOT:-/usr/share/nginx/html}"
JAMS_DIR="${JAMS_DIR:-$HTML_ROOT/jams}"
MANIFEST="${MANIFEST:-$HTML_ROOT/jams/SHA256SUMS}"
SERVICE_NAME="${SERVICE_NAME:-nockchain}"
NOCKCHAIN_RPC="${NOCKCHAIN_RPC:-localhost:5556}"
NOCKCHAIN_BIN="${NOCKCHAIN_BIN:-/root/.cargo/bin/nockchain}"
NOCKCHAIN_DIR="${NOCKCHAIN_DIR:-/root/nockchain}"

HASHER_BIN=""
SERVICE_WAS_STOPPED_BY_SCRIPT=0

usage() {
  cat <<EOF
Usage:
  $(basename "$0") jam     # Export a new state jam, then hash
  $(basename "$0") hash    # Generate/update manifest only
  $(basename "$0") check   # Verify files against manifest

Optional env overrides:
  HTML_ROOT=/usr/share/nginx/html
  JAMS_DIR=/usr/share/nginx/html/jams
  MANIFEST=/usr/share/nginx/html/SHA256SUMS
  SERVICE_NAME=nockchain
  NOCKCHAIN_RPC=localhost:5556
  NOCKCHAIN_BIN=/root/.cargo/bin/nockchain
  NOCKCHAIN_DIR=/root/nockchain
EOF
}

ensure_hasher() {
  if command -v sha256sum >/dev/null 2>&1; then
    HASHER_BIN="sha256sum"
  elif command -v shasum >/dev/null 2>&1; then
    HASHER_BIN="shasum"
  else
    echo "ERROR: Need sha256sum or shasum installed." >&2
    exit 1
  fi
}

hash_file() {
  local file="$1"
  if [[ "$HASHER_BIN" == "sha256sum" ]]; then
    sha256sum "$file" | awk '{print $1}'
  else
    shasum -a 256 "$file" | awk '{print $1}'
  fi
}

get_tip_block() {
  if ! command -v grpcurl >/dev/null 2>&1; then
    echo "ERROR: grpcurl is required. Install: https://github.com/fullstorydev/grpcurl" >&2
    exit 1
  fi

  local response
  response=$(grpcurl -plaintext \
    -d '{"page":{"clientPageItemsLimit":1}}' \
    "$NOCKCHAIN_RPC" \
    nockchain.public.v2.NockchainBlockService/GetBlocks 2>&1)

  local block_number
  block_number=$(echo "$response" | grep -oP '"currentHeight"\s*:\s*"\K[0-9]+' || true)

  if [[ -z "$block_number" ]]; then
    echo "ERROR: Could not parse block height from gRPC response:" >&2
    echo "$response" >&2
    exit 1
  fi

  echo "$block_number"
}

export_jam() {
  local block_number="${1:?block_number required}"
  echo "Current tip block: $block_number"

  mkdir -p "$JAMS_DIR"
  local jam_path="$JAMS_DIR/${block_number}.jam"

  if [[ -f "$jam_path" ]]; then
    echo "Jam already exists: $jam_path (skipping export)"
    return 0
  fi

  echo "Exporting state jam to: $jam_path (from $NOCKCHAIN_DIR)"
  (cd "$NOCKCHAIN_DIR" && "$NOCKCHAIN_BIN" --export-state-jam "$jam_path")
  echo "Exported: $jam_path"
}

stop_service_and_wait() {
  echo "Stopping service: $SERVICE_NAME"
  systemctl stop "$SERVICE_NAME" </dev/null >/dev/null 2>&1
  while systemctl is-active --quiet "$SERVICE_NAME"; do
    sleep 1
  done
  SERVICE_WAS_STOPPED_BY_SCRIPT=1
  echo "Service stopped: $SERVICE_NAME"
}

start_service_and_wait() {
  echo "Starting service: $SERVICE_NAME"
  systemctl start "$SERVICE_NAME" </dev/null >/dev/null 2>&1
  until systemctl is-active --quiet "$SERVICE_NAME"; do
    sleep 1
  done
  SERVICE_WAS_STOPPED_BY_SCRIPT=0
  echo "Service active: $SERVICE_NAME"
}

cleanup() {
  echo "DEBUG: cleanup() entered (SERVICE_WAS_STOPPED_BY_SCRIPT=$SERVICE_WAS_STOPPED_BY_SCRIPT)"
  # Always restart if this script stopped it.
  if [[ "$SERVICE_WAS_STOPPED_BY_SCRIPT" -eq 1 ]]; then
    start_service_and_wait || true
  fi
  echo "DEBUG: cleanup() exiting"
}

collect_files() {
  [[ -f "$HTML_ROOT/index.html" ]] && echo "$HTML_ROOT/index.html"
  [[ -f "$HTML_ROOT/privacy.html" ]] && echo "$HTML_ROOT/privacy.html"

  if [[ -d "$JAMS_DIR" ]]; then
    for f in "$JAMS_DIR"/*.jam; do
      [[ -f "$f" ]] && echo "$f"
    done
  fi
}

write_manifest() {
  local tmp
  tmp="$(mktemp)"

  while IFS= read -r f; do
    local rel hash
    rel="${f#"$HTML_ROOT"/}"
    hash="$(hash_file "$f")"
    printf "%s  %s\n" "$hash" "$rel" >> "$tmp"
  done < <(collect_files | sort)

  if [[ ! -s "$tmp" ]]; then
    echo "ERROR: No files found to hash." >&2
    rm -f "$tmp"
    exit 1
  fi

  mv "$tmp" "$MANIFEST"
  chmod 644 "$MANIFEST"
  echo "Manifest written: $MANIFEST"
}

export_and_hash() {
  local block_number="${1:?block_number required}"
  export_jam "$block_number"
  write_manifest
}

check_manifest() {
  [[ -f "$MANIFEST" ]] || { echo "ERROR: Manifest not found: $MANIFEST" >&2; exit 1; }

  local ok=1
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue

    if [[ ! "$line" =~ ^([a-fA-F0-9]{64})[[:space:]]+(.+)$ ]]; then
      echo "WARN: Skipping invalid manifest line: $line"
      continue
    fi

    local expected rel file actual
    expected="${BASH_REMATCH[1],,}"
    rel="${BASH_REMATCH[2]}"
    file="$HTML_ROOT/$rel"

    if [[ ! -f "$file" ]]; then
      echo "MISSING: $rel"
      ok=0
      continue
    fi

    actual="$(hash_file "$file")"
    actual="${actual,,}"
    if [[ "$actual" == "$expected" ]]; then
      echo "OK: $rel"
    else
      echo "FAIL: $rel"
      echo "  expected: $expected"
      echo "  actual:   $actual"
      ok=0
    fi
  done < "$MANIFEST"

  if [[ -d "$JAMS_DIR/.data.nockchain" ]]; then
    echo "WARN: $JAMS_DIR/.data.nockchain exists (should not be web-exposed)."
  fi

  [[ $ok -eq 1 ]] && echo "Integrity check PASSED" || { echo "Integrity check FAILED"; exit 2; }
}

run_with_service_cycle() {
  trap cleanup EXIT
  stop_service_and_wait
  "$@"
  start_service_and_wait
  echo "DEBUG: run_with_service_cycle done"
}

main() {
  [[ $# -eq 1 ]] || { usage; exit 1; }
  ensure_hasher

  case "$1" in
    jam)
      local tip
      tip="$(get_tip_block)"
      echo "Fetched tip block $tip while service is still running"
      run_with_service_cycle export_and_hash "$tip"
      ;;
    hash)
      run_with_service_cycle write_manifest
      ;;
    check)
      run_with_service_cycle check_manifest
      ;;
    *)
      usage
      exit 1
      ;;
  esac
}

main "$@"
echo "DEBUG: main returned; exiting script"
exit 0
