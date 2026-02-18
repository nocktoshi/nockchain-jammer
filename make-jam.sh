#!/usr/bin/env bash
set -euo pipefail

# Default paths (override with env vars if needed)
HTML_ROOT="${HTML_ROOT:-/usr/share/nginx/html}"
JAMS_DIR="${JAMS_DIR:-$HTML_ROOT/jams}"
MANIFEST="${MANIFEST:-$HTML_ROOT/jams/SHA256SUMS}"
SERVICE_NAME="${SERVICE_NAME:-nockchain}"

HASHER_BIN=""
SERVICE_WAS_STOPPED_BY_SCRIPT=0

usage() {
  cat <<EOF
Usage:
  $(basename "$0") hash    # Generate/update manifest
  $(basename "$0") check   # Verify files against manifest

Optional env overrides:
  HTML_ROOT=/usr/share/nginx/html
  JAMS_DIR=/usr/share/nginx/html/jams
  MANIFEST=/usr/share/nginx/html/SHA256SUMS
  SERVICE_NAME=nockchain
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

stop_service_and_wait() {
  echo "Stopping service: $SERVICE_NAME"
  systemctl stop "$SERVICE_NAME"
  while systemctl is-active --quiet "$SERVICE_NAME"; do
    sleep 1
  done
  SERVICE_WAS_STOPPED_BY_SCRIPT=1
  echo "Service stopped: $SERVICE_NAME"
}

start_service_and_wait() {
  echo "Starting service: $SERVICE_NAME"
  systemctl start "$SERVICE_NAME"
  until systemctl is-active --quiet "$SERVICE_NAME"; do
    sleep 1
  done
  SERVICE_WAS_STOPPED_BY_SCRIPT=0
  echo "Service active: $SERVICE_NAME"
}

cleanup() {
  # Always restart if this script stopped it.
  if [[ "$SERVICE_WAS_STOPPED_BY_SCRIPT" -eq 1 ]]; then
    start_service_and_wait || true
  fi
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
  echo "Manifest written: $MANIFEST"
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
}

main() {
  [[ $# -eq 1 ]] || { usage; exit 1; }
  ensure_hasher

  case "$1" in
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
