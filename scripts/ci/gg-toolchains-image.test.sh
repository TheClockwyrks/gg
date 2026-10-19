#!/usr/bin/env bash
# Table test for gg-toolchains-image.sh. Run it directly:
# scripts/ci/gg-toolchains-image.test.sh
#
# Each case copies the script and tcab-lib.sh into a scratch tree whose
# crates/gg/Cargo.toml declares a version and whose rust-version.sh pins a
# compiler, and hands it a stub `docker` that records each call. The subjects
# are the version check, the tags each command builds, pushes and fuses, and
# the retry of a registry round trip.
set -uo pipefail

CI_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly CI_DIR
pass=0
fail=0

ok() { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
bad() {
	fail=$((fail + 1))
	printf 'FAIL  %s\n' "$1"
	shift
	printf '        %s\n' "$@"
}

check_equal() { # label expected actual
	if [ "$2" = "$3" ]; then
		ok "$1"
	else
		bad "$1" "expected: $2" "got:      ${3:-<empty>}"
	fi
}

check_contains() { # label needle haystack
	if grep -qF -- "$2" <<<"$3"; then
		ok "$1"
	else
		bad "$1" "expected output containing: $2" "got: ${3:-<empty>}"
	fi
}

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

case "$(uname -m)" in
	x86_64 | amd64) arch=amd64 ;;
	*) arch=arm64 ;;
esac
readonly arch

# A tree declaring <version>, with a docker stub that fails its first
# <failures> calls of every kind and records each call in docker.log.
tree() { # dir version failures
	mkdir -p "$1/scripts/ci" "$1/crates/gg" "$1/packages/gg-sandbox-rust" "$1/bin"
	cp "$CI_DIR/gg-toolchains-image.sh" "$CI_DIR/tcab-lib.sh" "$1/scripts/ci/"
	printf '[package]\nname = "test-cabinet-gg"\nversion = "%s"\n\n[dependencies]\nversion = "9.9.9"\n' "$2" \
		>"$1/crates/gg/Cargo.toml"
	printf 'GG_RUST_VERSION=1.99.0\nGG_RUST_TARGET=wasm32-wasip1\n' >"$1/packages/gg-sandbox-rust/rust-version.sh"
	cat >"$1/bin/docker" <<STUB
#!/usr/bin/env bash
echo "DOCKER_BUILDKIT=\${DOCKER_BUILDKIT:-} \$*" >>"$1/docker.log"
count="\$(cat "$1/docker.count" 2>/dev/null || echo 0)"
echo "\$((count + 1))" >"$1/docker.count"
[ "\$count" -ge "$3" ]
STUB
	chmod +x "$1/bin/docker"
}

run() { # dir args...
	local dir="$1"
	shift
	PATH="$dir/bin:$PATH" REGISTRY_RETRY_DELAY=0 "$dir/scripts/ci/gg-toolchains-image.sh" "$@" 2>&1
}

echo "build: the version the crate declares, for this architecture"
tree "$scratch/build" 0.7.1 0
out="$(run "$scratch/build" build 0.7.1)"
check_equal "build exits 0" 0 "$?"
log="$(cat "$scratch/build/docker.log")"
check_contains "builds the toolchains Dockerfile from the root" \
	"DOCKER_BUILDKIT=1 build --build-arg RUST_VERSION=1.99.0 --build-arg RUST_TARGET=wasm32-wasip1 -t testcabinet.azurecr.io/test-cabinet-gg-toolchains:0.7.1-${arch} -f containers/gg-toolchains/Dockerfile ." \
	"$log"
check_equal "pushes nothing without --push" 1 "$(wc -l <"$scratch/build/docker.log" | tr -d ' ')"

echo "build --push: pushes the architecture's tag"
tree "$scratch/push" 0.7.1 0
out="$(run "$scratch/push" build 0.7.1 --push)"
check_equal "build --push exits 0" 0 "$?"
check_contains "pushes the tag it built" "push testcabinet.azurecr.io/test-cabinet-gg-toolchains:0.7.1-${arch}" \
	"$(cat "$scratch/push/docker.log")"

echo "manifest: fuses both architectures under the version"
tree "$scratch/manifest" 0.7.1 0
out="$(run "$scratch/manifest" manifest 0.7.1)"
check_equal "manifest exits 0" 0 "$?"
log="$(cat "$scratch/manifest/docker.log")"
check_contains "creates the multi-arch tag from both" \
	"buildx imagetools create --tag testcabinet.azurecr.io/test-cabinet-gg-toolchains:0.7.1 testcabinet.azurecr.io/test-cabinet-gg-toolchains:0.7.1-amd64 testcabinet.azurecr.io/test-cabinet-gg-toolchains:0.7.1-arm64" \
	"$log"
check_contains "inspects what it fused" "buildx imagetools inspect testcabinet.azurecr.io/test-cabinet-gg-toolchains:0.7.1" "$log"

echo "manifest: a registry round trip that fails twice is retried"
tree "$scratch/retry" 0.7.1 2
out="$(run "$scratch/retry" manifest 0.7.1)"
check_equal "retried manifest exits 0" 0 "$?"
check_equal "the create ran three times, the inspect once" 4 "$(wc -l <"$scratch/retry/docker.log" | tr -d ' ')"

echo "manifest: three failures fail the command"
tree "$scratch/down" 0.7.1 3
out="$(run "$scratch/down" manifest 0.7.1)"
check_equal "an unreachable registry fails" 1 "$?"

echo "refusals"
tree "$scratch/other" 0.7.0 0
out="$(run "$scratch/other" build 0.7.1)"
check_equal "a version the crate does not declare is refused" 1 "$?"
check_contains "names the crate's version" "crates/gg/Cargo.toml declares 0.7.0, not 0.7.1" "$out"
check_equal "nothing ran" "" "$(cat "$scratch/other/docker.log" 2>/dev/null)"
out="$(run "$scratch/other" manifest v0.7.0)"
check_equal "a tag name is refused" 1 "$?"
check_contains "says it is not a version" "'v0.7.0' is not a release version" "$out"
out="$(run "$scratch/other" build 0.7.0 --load)"
check_equal "an unknown flag is refused" 2 "$?"
out="$(run "$scratch/other" publish 0.7.0)"
check_equal "an unknown command is refused" 2 "$?"
out="$(run "$scratch/other" build)"
check_equal "a missing version is refused" 2 "$?"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
