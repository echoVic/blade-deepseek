#!/usr/bin/env sh
set -eu

ORCA_REPO="${ORCA_REPO:-echoVic/blade-deepseek}"
ORCA_VERSION="${ORCA_VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

say() {
  printf '%s\n' "$*"
}

err() {
  printf 'orca install: %s\n' "$*" >&2
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    err "missing required command: $1"
    exit 1
  fi
}

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *)
      err "unsupported operating system: $os"
      exit 1
      ;;
  esac

  case "$arch" in
    arm64|aarch64) arch_part="aarch64" ;;
    x86_64|amd64) arch_part="x86_64" ;;
    *)
      err "unsupported architecture: $arch"
      exit 1
      ;;
  esac

  printf '%s-%s' "$arch_part" "$os_part"
}

download() {
  url="$1"
  out="$2"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$out"
  else
    err "missing required command: curl or wget"
    exit 1
  fi
}

sha256_file() {
  file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    err "missing required command: shasum or sha256sum"
    exit 1
  fi
}

version_path() {
  if [ "$ORCA_VERSION" = "latest" ]; then
    printf 'latest/download'
  else
    case "$ORCA_VERSION" in
      v*) printf 'download/%s' "$ORCA_VERSION" ;;
      *) printf 'download/v%s' "$ORCA_VERSION" ;;
    esac
  fi
}

main() {
  need_cmd uname
  need_cmd tar
  need_cmd awk
  need_cmd mktemp

  target="$(detect_target)"
  archive="orca-${target}.tar.gz"
  checksum="${archive}.sha256"
  release_path="$(version_path)"
  base_url="https://github.com/${ORCA_REPO}/releases/${release_path}"
  tmp_dir="$(mktemp -d)"

  cleanup() {
    rm -rf "$tmp_dir"
  }
  trap cleanup EXIT INT TERM

  say "Installing Orca for ${target}"
  say "Downloading ${base_url}/${archive}"

  download "${base_url}/${archive}" "${tmp_dir}/${archive}"
  download "${base_url}/${checksum}" "${tmp_dir}/${checksum}"

  expected="$(awk '{print $1}' "${tmp_dir}/${checksum}")"
  actual="$(sha256_file "${tmp_dir}/${archive}")"

  if [ "$expected" != "$actual" ]; then
    err "checksum mismatch for ${archive}"
    err "expected: ${expected}"
    err "actual:   ${actual}"
    exit 1
  fi

  mkdir -p "$INSTALL_DIR"
  tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"

  if [ ! -f "${tmp_dir}/orca" ]; then
    err "archive did not contain orca binary"
    exit 1
  fi

  install_path="${INSTALL_DIR}/orca"
  mv "${tmp_dir}/orca" "$install_path"
  chmod +x "$install_path"

  say "Installed ${install_path}"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      say "Add ${INSTALL_DIR} to your PATH to run orca from any directory."
      ;;
  esac

  if "$install_path" --version >/dev/null 2>&1; then
    "$install_path" --version
  fi
}

main "$@"
