#!/usr/bin/env bash
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
# Dry run (prints old → new SHAs, writes nothing):
#   bash scripts/pin-model-shas.sh
#
# Apply (rewrites src/audio/manifest.json in place):
#   bash scripts/pin-model-shas.sh --apply
#
# Dependencies: `curl`, `shasum` (ships with macOS), `jq`. Plain bash
# 3.2 compatible — no associative arrays, no `readarray`.

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
  # On 404 / network error, print a SKIP marker and return success so
  # the loop continues. The caller treats an empty string as "leave
  # the existing value alone" — useful when a manifest entry has a URL
  # that hasn't stabilized yet (e.g. a Phase-4 TTS model).
  if ! curl --fail --location --silent --show-error "${url}" -o "${out}" 2>/dev/null; then
    echo "  ⚠  download failed for ${basename}; leaving its SHA untouched" >&2
    return 0
  fi
  shasum -a 256 "${out}" | awk '{print $1}'
}

# We stage updates by writing a fresh `jq` invocation per asset to a tmp
# copy of the manifest, then `mv` it over at the end if `--apply`.
STAGE_MANIFEST="${TMP_DIR}/manifest.staged.json"
cp "${MANIFEST}" "${STAGE_MANIFEST}"

# --- STT models ---
echo "STT models:"
stt_keys="$(jq -r '.stt_models | keys[]' "${MANIFEST}")"
while IFS= read -r key; do
  [[ -z "${key}" ]] && continue
  url=$(jq -r --arg k "${key}" '.stt_models[$k].url' "${MANIFEST}")
  old=$(jq -r --arg k "${key}" '.stt_models[$k].sha256' "${MANIFEST}")
  new="$(download_and_hash "${url}")"
  if [[ -z "${new}" ]]; then
    printf "  %-20s  %s → (skipped)\n" "${key}" "${old:0:12}…"
    continue
  fi
  printf "  %-20s  %s → %s\n" "${key}" "${old:0:12}…" "${new}"
  jq --arg k "${key}" --arg sha "${new}" \
    '.stt_models[$k].sha256 = $sha' "${STAGE_MANIFEST}" > "${STAGE_MANIFEST}.work"
  mv "${STAGE_MANIFEST}.work" "${STAGE_MANIFEST}"
done <<< "${stt_keys}"

# --- TTS models ---
echo
echo "TTS models:"
tts_keys="$(jq -r '.tts_models | keys[]' "${MANIFEST}")"
while IFS= read -r key; do
  [[ -z "${key}" ]] && continue

  # ONNX
  onnx_url=$(jq -r --arg k "${key}" '.tts_models[$k].onnx_url' "${MANIFEST}")
  old_onnx=$(jq -r --arg k "${key}" '.tts_models[$k].onnx_sha256' "${MANIFEST}")
  new_onnx="$(download_and_hash "${onnx_url}")"
  if [[ -n "${new_onnx}" ]]; then
    printf "  %-20s  onnx:       %s → %s\n" "${key}" "${old_onnx:0:12}…" "${new_onnx}"
    jq --arg k "${key}" --arg sha "${new_onnx}" \
      '.tts_models[$k].onnx_sha256 = $sha' "${STAGE_MANIFEST}" > "${STAGE_MANIFEST}.work"
    mv "${STAGE_MANIFEST}.work" "${STAGE_MANIFEST}"
  else
    printf "  %-20s  onnx:       %s → (skipped)\n" "${key}" "${old_onnx:0:12}…"
  fi

  # Tokenizer
  tok_url=$(jq -r --arg k "${key}" '.tts_models[$k].tokenizer_url // empty' "${MANIFEST}")
  if [[ -n "${tok_url}" ]]; then
    old_tok=$(jq -r --arg k "${key}" '.tts_models[$k].tokenizer_sha256 // empty' "${MANIFEST}")
    new_tok="$(download_and_hash "${tok_url}")"
    if [[ -n "${new_tok}" ]]; then
      printf "  %-20s  tokenizer:  %s → %s\n" "${key}" "${old_tok:0:12}…" "${new_tok}"
      jq --arg k "${key}" --arg sha "${new_tok}" \
        '.tts_models[$k].tokenizer_sha256 = $sha' "${STAGE_MANIFEST}" > "${STAGE_MANIFEST}.work"
      mv "${STAGE_MANIFEST}.work" "${STAGE_MANIFEST}"
    else
      printf "  %-20s  tokenizer:  %s → (skipped)\n" "${key}" "${old_tok:0:12}…"
    fi
  fi

  # Per-voice files (new schema)
  voice_keys="$(jq -r --arg k "${key}" '.tts_models[$k].voices // {} | keys[]' "${MANIFEST}")"
  while IFS= read -r voice; do
    [[ -z "${voice}" ]] && continue
    v_url=$(jq -r --arg k "${key}" --arg v "${voice}" '.tts_models[$k].voices[$v].url' "${MANIFEST}")
    old_v=$(jq -r --arg k "${key}" --arg v "${voice}" '.tts_models[$k].voices[$v].sha256' "${MANIFEST}")
    new_v="$(download_and_hash "${v_url}")"
    if [[ -n "${new_v}" ]]; then
      printf "  %-20s  voice %-10s %s → %s\n" "${key}" "${voice}" "${old_v:0:12}…" "${new_v}"
      jq --arg k "${key}" --arg v "${voice}" --arg sha "${new_v}" \
        '.tts_models[$k].voices[$v].sha256 = $sha' "${STAGE_MANIFEST}" > "${STAGE_MANIFEST}.work"
      mv "${STAGE_MANIFEST}.work" "${STAGE_MANIFEST}"
    else
      printf "  %-20s  voice %-10s %s → (skipped)\n" "${key}" "${voice}" "${old_v:0:12}…"
    fi
  done <<< "${voice_keys}"
done <<< "${tts_keys}"

if [[ ${APPLY} -eq 0 ]]; then
  echo
  echo "Dry run only. Re-run with --apply to write to ${MANIFEST}."
  exit 0
fi

mv "${STAGE_MANIFEST}" "${MANIFEST}"
echo
echo "Updated ${MANIFEST}."
echo "Verify with: git diff src/audio/manifest.json && cargo test --lib audio::manifest"
