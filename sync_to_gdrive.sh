#!/usr/bin/env bash
set -euo pipefail

# === CONFIG ===
SRC_DIR="/usr/share/nginx/html/jams"     # your local source
REMOTE="gdrive:"                         # your rclone remote name
DEST_FOLDER_ID="1P9-XYfFE6gJi6rosFLd9sjGpi3AWkpMs"  # Drive folder ID
LOG_FILE="/var/log/rclone-jams-sync.log"

# rclone performance knobs (tune as desired for large files)
TRANSFERS=2
CHECKERS=8
CHUNK_SIZE="64M"

# === PREP ===
if [[ ! -d "$SRC_DIR" ]]; then
  echo "$(date -Is) Source directory not found: $SRC_DIR" | tee -a "$LOG_FILE"
  exit 1
fi

# Build a file list (relative to SRC_DIR) of the newest 2 *.jam files
TMP_LIST="$(mktemp)"
pushd "$SRC_DIR" >/dev/null

# Find newest two by modification time, output just filenames (no leading ./)
# %T@ = epoch mtime, %P = path relative to starting directory
mapfile -t NEWEST_JAMS < <(
  find . -maxdepth 1 -type f -name '*.jam' -printf '%T@ %P\n' \
  | sort -nr \
  | head -n 2 \
  | awk '{ $1=""; sub(/^ /,""); print }'
)

if (( ${#NEWEST_JAMS[@]} == 0 )); then
  echo "$(date -Is) No .jam files in $SRC_DIR. Nothing to do." | tee -a "$LOG_FILE"
  popd >/dev/null
  rm -f "$TMP_LIST"
  exit 0
fi

printf '%s\n' "${NEWEST_JAMS[@]}" > "$TMP_LIST"

echo "$(date -Is) Will keep these $(wc -l < "$TMP_LIST") newest .jam file(s):" | tee -a "$LOG_FILE"
printf '  - %s\n' "${NEWEST_JAMS[@]}" | tee -a "$LOG_FILE"

popd >/dev/null

# === DELETE FROM DRIVE FIRST (free quota before copying) ===
# rclone sync copies then deletes by default; we delete first so we don't run out of quota.
REMOTE_FILES=$(rclone lsf "$REMOTE" --drive-root-folder-id "$DEST_FOLDER_ID" --files-only 2>> "$LOG_FILE" || true)
while IFS= read -r f; do
  [[ -z "$f" ]] && continue
  f="${f#./}"
  f="${f#/}"
  if ! grep -Fxq "$f" "$TMP_LIST" 2>/dev/null; then
    echo "$(date -Is) Deleting from Drive (free quota): $f" | tee -a "$LOG_FILE"
    rclone deletefile "$REMOTE$f" --drive-root-folder-id "$DEST_FOLDER_ID" \
      --drive-use-trash=false \
      --log-file "$LOG_FILE" --log-level INFO 2>> "$LOG_FILE" || true

    sleep 10s
    if rclone lsf "$REMOTE" --drive-root-folder-id "$DEST_FOLDER_ID" --files-only 2>> "$LOG_FILE" | grep -Fxq "$f" 2>/dev/null; then
      echo "$(date -Is) Failed to delete from Drive: $f" | tee -a "$LOG_FILE"
    fi
  fi
done <<< "$REMOTE_FILES"

# === SYNC THE 2 NEWEST FILES TO DRIVE ===
rclone sync "$SRC_DIR" "$REMOTE" \
  --drive-root-folder-id "$DEST_FOLDER_ID" \
  --drive-use-trash=false \
  --files-from "$TMP_LIST" \
  --transfers "$TRANSFERS" \
  --checkers "$CHECKERS" \
  --drive-chunk-size "$CHUNK_SIZE" \
  --log-file "$LOG_FILE" --log-level INFO \
  --progress

rm -f "$TMP_LIST"
echo "$(date -Is) Sync complete." | tee -a "$LOG_FILE"