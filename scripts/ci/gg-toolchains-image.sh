#!/usr/bin/env bash
# Builds, pushes and fuses gg's toolchains image, the tree every `-gg` run image
# copies, published beside a release.
#
#   scripts/ci/gg-toolchains-image.sh build <version> [--push]
#   scripts/ci/gg-toolchains-image.sh manifest <version>
#
# `build` builds containers/gg-toolchains/Dockerfile for this machine's
# architecture and tags it `<registry>/test-cabinet-gg-toolchains:<version>-<arch>`,
# pushing it with --push. `manifest` fuses the two architectures' tags into the
# multi-arch `<registry>/test-cabinet-gg-toolchains:<version>`, the reference the
# superrepo's `-gg` run images copy `/opt/gg` from for the release its driver
# pins. <version> is the release version without the tag's `v`, which is the
# version crates/gg/Cargo.toml declares and the one `gg --version` reports, so
# one string names a release's binaries, its reference documents and this
# image. Either command refuses any other version.
#
# gg's pipeline runs `build --push` on each architecture's agent and then
# `manifest` on a `v*` tag, in its `release` stage (.azure/project/stages.yml),
# before the binaries are uploaded, so a release whose image did not build
# publishes nothing. The caller is logged in to the registry.
#
# Every toolchain's version is read by the image from the package that owns it
# (each arm's block COPYs its version file and runs the same installer a
# developer runs), except Rust's, which has no installer to run: the compiler
# and wasm target come from packages/gg-sandbox-rust/rust-version.sh, which reads
# rust-toolchain.toml, and are passed as build arguments the Dockerfile leaves
# undefaulted.
set -euo pipefail
# shellcheck source=scripts/ci/tcab-lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/tcab-lib.sh"

readonly IMAGE="test-cabinet-gg-toolchains"
readonly DOCKER="${DOCKER:-docker}"
readonly MANIFEST="crates/gg/Cargo.toml"

usage() {
	echo "usage: scripts/ci/gg-toolchains-image.sh build <version> [--push]" >&2
	echo "       scripts/ci/gg-toolchains-image.sh manifest <version>" >&2
	exit 2
}

# The version crates/gg/Cargo.toml declares for the package.
crate_version() {
	awk '/^\[/ { in_package = ($0 == "[package]") } in_package && /^version = "/ { gsub(/"/, "", $3); print $3; exit }' "$MANIFEST"
}

# A registry round trip, three times ten seconds apart: the registry's token
# exchange times out on its side now and then, which says nothing of the build.
registry_retry() {
	local attempt
	for attempt in 1 2 3; do
		"$@" && return 0
		[[ $attempt -lt 3 ]] && sleep "${REGISTRY_RETRY_DELAY:-10}"
	done
	return 1
}

[[ $# -ge 2 ]] || usage
readonly COMMAND="$1"
readonly VERSION="$2"
shift 2

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
	echo "gg-toolchains-image: '${VERSION}' is not a release version (no leading 'v')" >&2
	exit 1
fi
declared="$(crate_version)"
if [[ "$declared" != "$VERSION" ]]; then
	echo "gg-toolchains-image: ${MANIFEST} declares ${declared:-no version}, not ${VERSION}" >&2
	exit 1
fi

readonly REPO="${CI_REGISTRY}/${IMAGE}"

case "$COMMAND" in
	build)
		push=false
		if [[ $# -eq 1 && "$1" == --push ]]; then
			push=true
		elif [[ $# -ne 0 ]]; then
			usage
		fi
		tag="${REPO}:${VERSION}-$(ci_arch)"
		# shellcheck source=packages/gg-sandbox-rust/rust-version.sh
		source packages/gg-sandbox-rust/rust-version.sh
		log "building ${tag} (Rust ${GG_RUST_VERSION}, ${GG_RUST_TARGET})"
		DOCKER_BUILDKIT=1 "$DOCKER" build \
			--build-arg "RUST_VERSION=${GG_RUST_VERSION}" \
			--build-arg "RUST_TARGET=${GG_RUST_TARGET}" \
			-t "$tag" \
			-f containers/gg-toolchains/Dockerfile .
		if [[ "$push" == true ]]; then
			log "pushing ${tag}"
			registry_retry "$DOCKER" push "$tag"
		fi
		;;
	manifest)
		[[ $# -eq 0 ]] || usage
		log "assembling ${REPO}:${VERSION} from :${VERSION}-amd64 and :${VERSION}-arm64"
		registry_retry "$DOCKER" buildx imagetools create --tag "${REPO}:${VERSION}" \
			"${REPO}:${VERSION}-amd64" "${REPO}:${VERSION}-arm64"
		registry_retry "$DOCKER" buildx imagetools inspect "${REPO}:${VERSION}"
		;;
	*)
		usage
		;;
esac
