#!/bin/bash
# Pin real SHA-256 hashes for every audio asset in src/audio/manifest.json.
#
# Why this is a separate, manual step:
#   Hugging Face doesn't expose a stable SHA-256 in HTTP headers (they
#   use git-LFS / Xet checksums that don't survive the CDN hop as a
#   Rust-crate-friendly constant). To pin an asset we have to download
#   it once, compute shasum locally, and paste the hex into the JSON.
#
# Run this:
#   - once before the first release that enables `TELEGRAM_STT=on` by default
#   - again whenever an asset URL or the upstream file changes
#
# The script downloads everything to a temp dir, computes SHA-256 for
# each, and prints `sed`-style updates the user can eyeball + apply
# with `bash scripts/pin-model-shas.sh --apply`.
#
# Dependencies: `curl`, `shasum` (ships with macOS), `jq`.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="${REPO_ROOT}/src/audio/manifest.json"
APPLY=0
TMP_DIR="${TMPDIR:-/tmp}/tebis-pin-shas-$$"

if [[ "${1:-}" == "--apply" ]]; then
  APPLY=1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required (brew install jq / apt install jq)" >&2
  exit 2
fi
if ! command -v shasum >/dev/null 2>&1; then
  echo "error: shasum is required (bundled on macOS; coreutils provides sha256sum on Linux)" >&2
  exit 2
fi

mkdir -p "${TMP_DIR}"
trap 'rm -rf "${TMP_DIR}"' EXIT

download_and_hash() {
  local url="$1"
  local basename
  basename="$(basename "${url%%\?*}")"
  local out="${TMP_DIR}/${basename}"

  echo "  fetching ${basename} from ${url}…" >&2
  curl --fail --location --silent --show-error "${url}" -o "${out}"
  shasum -a 256 "${out}" | awk '{print $1}'
}

# Enumerate STT models: key + url + sha256 placeholder.
readarray -t stt_keys < <(jq -r '.stt_models | keys[]' "${MANIFEST}")
echo "STT models (${#stt_keys[@]}):"
declare -A stt_updates
for key in "${stt_keys[@]}"; do
  url=$(jq -r ".stt_models[\"${key}\"].url" "${MANIFEST}")
  old=$(jq -r ".stt_models[\"${key}\"].sha256" "${MANIFEST}")
  new=$(download_and_hash "${url}")
  stt_updates["${key}"]="${new}"
  printf "  %-20s  %s → %s\n" "${key}" "${old:0:12}…" "${new}"
done

# Enumerate TTS models: key + onnx_url + voices_url + both shas.
readarray -t tts_keys < <(jq -r '.tts_models | keys[]' "${MANIFEST}")
echo
echo "TTS models (${#tts_keys[@]}):"
declare -A tts_onnx_updates
declare -A tts_voices_updates
for key in "${tts_keys[@]}"; do
  onnx_url=$(jq -r ".tts_models[\"${key}\"].onnx_url" "${MANIFEST}")
  voices_url=$(jq -r ".tts_models[\"${key}\"].voices_url" "${MANIFEST}")
  old_onnx=$(jq -r ".tts_models[\"${key}\"].onnx_sha256" "${MANIFEST}")
  old_voices=$(jq -r ".tts_models[\"${key}\"].voices_sha256" "${MANIFEST}")
  new_onnx=$(download_and_hash "${onnx_url}")
  new_voices=$(download_and_hash "${voices_url}")
  tts_onnx_updates["${key}"]="${new_onnx}"
  tts_voices_updates["${key}"]="${new_voices}"
  printf "  %-20s  onnx:   %s → %s\n" "${key}" "${old_onnx:0:12}…" "${new_onnx}"
  printf "  %-20s  voices: %s → %s\n" "${key}" "${old_voices:0:12}…" "${new_voices}"
done

if [[ ${APPLY} -eq 0 ]]; then
  echo
  echo "Dry run only. Re-run with --apply to write to ${MANIFEST}."
  exit 0
fi

# Apply via jq in-place — safe atomic rewrite via mktemp + mv.
TMP_MANIFEST="$(mktemp "${MANIFEST}.XXXXXX")"
cp "${MANIFEST}" "${TMP_MANIFEST}"
for key in "${stt_keys[@]}"; do
  new="${stt_updates[${key}]}"
  jq --arg k "${key}" --arg sha "${new}" '.stt_models[$k].sha256 = $sha' "${TMP_MANIFEST}" \
    > "${TMP_MANIFEST}.work"
  mv "${TMP_MANIFEST}.work" "${TMP_MANIFEST}"
done
for key in "${tts_keys[@]}"; do
  new_onnx="${tts_onnx_updates[${key}]}"
  new_voices="${tts_voices_updates[${key}]}"
  jq --arg k "${key}" --arg o "${new_onnx}" --arg v "${new_voices}" \
    '.tts_models[$k].onnx_sha256 = $o | .tts_models[$k].voices_sha256 = $v' \
    "${TMP_MANIFEST}" > "${TMP_MANIFEST}.work"
  mv "${TMP_MANIFEST}.work" "${TMP_MANIFEST}"
done

mv "${TMP_MANIFEST}" "${MANIFEST}"
echo
echo "Updated ${MANIFEST}."
echo "Verify with: git diff src/audio/manifest.json && cargo test --lib audio::manifest"
