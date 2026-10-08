#!/bin/bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
RESET_COLOR='\033[0m'

write_installation_message() {
  echo -e "${BLUE}[INFO]${RESET_COLOR} $1"
}

log_success() {
  echo -e "${GREEN}[SUCCESS]${RESET_COLOR} $1"
}

log_error() {
  echo -e "${RED}[ERROR]${RESET_COLOR} $1" >&2
}

# Detect OS
operating_system_name="$(uname -s)"
case "${operating_system_name}" in
Linux*) operating_system_slug=linux ;;
Darwin*) operating_system_slug=darwin ;;
*)
  log_error "Unsupported operating system: ${operating_system_name}"
  exit 1
  ;;
esac

# Detect Architecture
architecture_name="$(uname -m)"
case "${architecture_name}" in
x86_64) architecture_slug=amd64 ;;
arm64) architecture_slug=arm64 ;;
aarch64) architecture_slug=arm64 ;;
*)
  log_error "Unsupported architecture: ${architecture_name}"
  exit 1
  ;;
esac

write_installation_message "Detected OS: ${operating_system_slug}, Architecture: ${architecture_slug}"

# The release this script installs
release_version="v0.5.0"

write_installation_message "Installing koshi version: ${release_version}"

# Construct download URL
# Naming convention: koshi-v{version}-{os}-{arch}.tar.gz
release_version_number="${release_version#v}"
archive_file_name="koshi-v${release_version_number}-${operating_system_slug}-${architecture_slug}.tar.gz"
release_archive_url="https://github.com/gohyuhan/koshi/releases/download/${release_version}/${archive_file_name}"

write_installation_message "Download URL: ${release_archive_url}"

# Create the staging directory. HUP, INT, and TERM end the script with
# status 1, which runs the EXIT trap.
staging_directory=$(mktemp -d)
trap 'rm -rf "${staging_directory}"' EXIT
trap 'exit 1' HUP INT TERM

# Download
write_installation_message "Downloading ${archive_file_name}..."
curl -fsSL "${release_archive_url}" -o "${staging_directory}/${archive_file_name}"

# Extract
write_installation_message "Extracting..."
tar -xzf "${staging_directory}/${archive_file_name}" -C "${staging_directory}"

# Check the extracted binary
binary_path="${staging_directory}/koshi"
if [ ! -f "${binary_path}" ]; then
  log_error "Binary 'koshi' not found in extracted archive."
  exit 1
fi

# Install
installation_directory="/usr/local/bin"
installed_binary_path="${installation_directory}/koshi"
staged_binary_path="${installation_directory}/koshi.koshi-update-$$"

write_installation_message "Installing to ${installed_binary_path}..."

# Run the staged copy with --version, as this user. A first line on standard
# output other than "koshi <release version number>" ends the script with
# status 1, which runs the EXIT trap. What the copy writes on standard error
# shows on the terminal.
validate_staged_binary() {
  staged_version_line="$("${staged_binary_path}" --version | head -n 1)"
  if [ "${staged_version_line}" != "koshi ${release_version_number}" ]; then
    log_error "The new koshi printed '${staged_version_line}' for --version, not 'koshi ${release_version_number}'"
    exit 1
  fi
}

# Create the install directory when it does not exist, through sudo when this
# user cannot create it.
if [ ! -d "${installation_directory}" ]; then
  mkdir -p "${installation_directory}" 2>/dev/null || sudo mkdir -p "${installation_directory}"
fi

# Copy the binary beside the installed one as koshi.koshi-update-<process id of
# this shell>, set mode 755 on the copy, check that the copy prints its
# version, and rename the copy over the installed one in one step. A failed
# step, HUP, INT, or TERM runs the EXIT trap, which deletes the copy and the
# staging directory. The installed binary is then the old one or the complete
# new one.
if [ -w "${installation_directory}" ]; then
  trap 'rm -rf "${staging_directory}"; rm -f "${staged_binary_path}"' EXIT
  cp "${binary_path}" "${staged_binary_path}"
  chmod 755 "${staged_binary_path}"
  validate_staged_binary
  mv -f "${staged_binary_path}" "${installed_binary_path}"
else
  write_installation_message "Requires sudo to install to ${installation_directory}"
  trap 'rm -rf "${staging_directory}"; if [ -e "${staged_binary_path}" ]; then sudo rm -f "${staged_binary_path}"; fi' EXIT
  sudo cp "${binary_path}" "${staged_binary_path}"
  sudo chmod 755 "${staged_binary_path}"
  validate_staged_binary
  sudo mv -f "${staged_binary_path}" "${installed_binary_path}"
fi

log_success "koshi installed successfully!"
write_installation_message "Run 'koshi --version' to verify."
