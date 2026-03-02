#!/usr/bin/env sh

set -eu

REPO="aplio/spectra"
GITHUB_API="https://api.github.com/repos/${REPO}/releases"

need_cmd() {
  cmd=$1
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "Missing required command: ${cmd}" >&2
    exit 1
  fi
}

need_cmd uname
need_cmd tr
need_cmd tar
need_cmd mktemp
need_cmd grep
need_cmd awk
need_cmd curl
need_cmd sed

os="$(uname -s)"
arch="$(uname -m)"

case "${os}" in
  Linux)
    case "${arch}" in
      x86_64|amd64)
        asset_suffix="linux-x86_64"
        ;;
      *)
        echo "Unsupported Linux architecture: ${arch}. Only x86_64 is supported." >&2
        exit 1
        ;;
    esac
    ;;
  Darwin)
    case "${arch}" in
      arm64|aarch64)
        asset_suffix="macos-arm64"
        ;;
      *)
        echo "Unsupported macOS architecture: ${arch}. Only arm64 is supported." >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "Unsupported operating system: ${os}. Only Linux and macOS are supported." >&2
    exit 1
    ;;
esac

if [ -n "${SPECTRA_VERSION:-}" ]; then
  release_api="${GITHUB_API}/tags/v${SPECTRA_VERSION#v}"
else
  release_api="${GITHUB_API}/latest"
fi
verify_checksums=1
if [ "${SPECTRA_SKIP_VERIFY:-0}" = "1" ]; then
  verify_checksums=0
fi

release_json="$(curl -fsSL "${release_api}")"
tag="$(printf '%s\n' "${release_json}" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"

if [ -z "${tag}" ]; then
  echo "Could not resolve a release tag from GitHub API endpoint: ${release_api}" >&2
  exit 1
fi

extract_asset() {
  pattern="$1"
  printf '%s\n' "$release_json" | awk "
    /\"name\"[[:space:]]*:[[:space:]]*\"/ {
      name=\$0
      sub(/^.*\\\"name\\\"[[:space:]]*:[[:space:]]*\\\"/, \"\", name)
      sub(/\\\".*/, \"\", name)
    }
    /\"browser_download_url\"[[:space:]]*:[[:space:]]*\"/ {
      if (name == \"\") next
      url=\$0
      sub(/^.*\\\"browser_download_url\\\"[[:space:]]*:[[:space:]]*\\\"/, \"\", url)
      sub(/\\\".*/, \"\", url)
      if (name ~ /${pattern}/) {
        print name \"|\" url
        exit
      }
      name=\"\"
    }
  "
}

archive_info="$(extract_asset "spectra-.*-${asset_suffix}\\.tar\\.gz$")"
archive="${archive_info%%|*}"
archive_url="${archive_info##*|}"

if [ -z "${archive}" ] || [ "${archive}" = "${archive_info}" ]; then
  echo "Could not locate a spectra ${asset_suffix} archive in release ${tag}." >&2
  exit 1
fi

if [ "${verify_checksums}" = "1" ]; then
  checksum_info="$(extract_asset "checksums\\.txt$")"
  checksum_file="${checksum_info%%|*}"
  checksum_url="${checksum_info##*|}"

  if [ -z "${checksum_file}" ] || [ "${checksum_file}" = "${checksum_info}" ]; then
    echo "Could not locate checksums.txt in release ${tag}. Proceeding without checksum verification."
    verify_checksums=0
  fi
fi

install_dir="${SPECTRA_BIN_DIR:-$HOME/.local/bin}"
tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "${tmpdir}"
}
trap cleanup EXIT INT TERM

archive_path="${tmpdir}/${archive}"
checksum_path="${tmpdir}/${checksum_file:-checksums.txt}"

curl -fsSL "${archive_url}" -o "${archive_path}"

if [ "${verify_checksums}" = "1" ]; then
  curl -fsSL "${checksum_url}" -o "${checksum_path}"

  if command -v sha256sum >/dev/null 2>&1; then
    expected="$(grep -F "${archive}" "${checksum_path}" | awk '{print $1}')"
    actual="$(sha256sum "${archive_path}" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    expected="$(grep -F "${archive}" "${checksum_path}" | awk '{print $1}')"
    actual="$(shasum -a 256 "${archive_path}" | awk '{print $1}')"
  else
    echo "Checksum verification requires sha256sum or shasum, which are missing." >&2
    echo "Set SPECTRA_SKIP_VERIFY=1 to skip verification." >&2
    exit 1
  fi

  if [ -z "${expected}" ]; then
    echo "Could not find checksum for ${archive} in checksums.txt." >&2
    exit 1
  fi

  if [ "${actual}" != "${expected}" ]; then
    echo "Checksum mismatch for ${archive}." >&2
    echo "Expected: ${expected}" >&2
    echo "Actual:   ${actual}" >&2
    exit 1
  fi
fi

tar -xzf "${archive_path}" -C "${tmpdir}"

if [ ! -x "${tmpdir}/spectra" ]; then
  echo "Downloaded archive did not contain a valid spectra binary." >&2
  exit 1
fi

mkdir -p "${install_dir}"
install_path="${install_dir}/spectra"
cp "${tmpdir}/spectra" "${install_path}"
chmod +x "${install_path}"

echo "Installed spectra ${tag} to ${install_path}"

case ":${PATH}:" in
  *:"${install_dir}":*)
    ;;
  *)
    echo "Note: ${install_dir} is not in your PATH."
    echo "Add it with: export PATH=\"${install_dir}:\$PATH\""
    ;;
esac
