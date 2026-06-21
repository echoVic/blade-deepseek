# Task 1 Report: Curl Installer

Implemented the curl-based installer exactly to the brief in `install.sh` and added the documented install snippet to `README.md`.

## What changed

- Added a portable shell installer that supports `ORCA_VERSION`, `INSTALL_DIR`, and `ORCA_REPO`.
- Detects `darwin` vs `linux` and `arm64` vs `x86_64`, then downloads the matching GitHub Release tarball and `.sha256` file.
- Verifies the archive checksum before extracting and installs the `orca` binary into the chosen directory.
- Added the curl install command and version-pinning example to the README installation section.

## Verification

- `sh -n install.sh`
- `tmp="$(mktemp -d)"; INSTALL_DIR="$tmp/bin" ORCA_VERSION=0.1.1 ./install.sh; "$tmp/bin/orca" --version`

## Result

- `install.sh` installed Orca successfully into a temporary directory.
- The installed binary reported `orca 0.1.1`.
- No concerns to flag.

## Review fix

- Corrected the pinned curl install example in `README.md` so `INSTALL_DIR` and `ORCA_VERSION` are passed to the `sh` process on the right side of the pipe.
