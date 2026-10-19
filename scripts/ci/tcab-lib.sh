# shellcheck shell=bash
# gg's shared helpers for its own CI scripts under scripts/ci/.
#
# The kit renders some of the scripts beside this file (the mirror, the feed,
# the package publish, the target prune), and those stand alone. gg's own
# scripts, which build, test and release gg and install its toolchains, source
# this one. It is named for the project, as the superrepo's is, so a kit update
# never renders over it.
#
# Sourced (not executed) by each script. It resolves the repository root from
# this file's own location and changes into it, so every CI script behaves
# identically regardless of the directory it is invoked from. The pipeline
# calls these scripts, and a developer reproduces any of its jobs by running
# the same script locally.

# Resolve the repo root two levels up from scripts/ci/ and work from there.
CI_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$CI_LIB_DIR/../.." && pwd)"
readonly CI_LIB_DIR REPO_ROOT
cd "$REPO_ROOT" || exit 1

# Print a labelled step header so the CI logs are easy to scan.
log() {
	printf '\n\033[1;34m==> %s\033[0m\n' "$*"
}

# The Test Cabinet's Azure Container Registry, where gg's release pushes its
# toolchains image.
readonly CI_REGISTRY="testcabinet.azurecr.io"
export CI_REGISTRY

# The architecture name an image is published under for this machine: amd64 or arm64.
ci_arch() {
	case "$(uname -m)" in
		x86_64 | amd64) echo amd64 ;;
		aarch64 | arm64) echo arm64 ;;
		*)
			echo "unsupported architecture '$(uname -m)'" >&2
			return 1
			;;
	esac
}
