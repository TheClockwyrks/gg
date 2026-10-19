#!/usr/bin/env bash
# Verifies that every file a Dockerfile COPYs out of the build context survives
# `.dockerignore`, the gate `build-context` (ci/gates/build-context.py) runs.
#
# WHY THIS EXISTS. `.dockerignore` is an ALLOWLIST — `*`, then explicit `!`
# re-inclusions — so the default answer for any path is "not in the context". A
# `COPY` whose source was never re-included therefore does not warn: the build dies
# with `failed to compute cache key: "/path": not found`, preceded by the giveaway
# `transferring context: 2B`. Nothing else in this repository can see that: the Rust
# suite and the lints pass on a tree whose image cannot be built at all, and the
# image is built only on a release tag, after the gates. This is the superrepo's
# gate of the same name, cut down to what gg's one image needs.
#
# What it checks, per tracked Dockerfile (today `containers/gg-toolchains/Dockerfile`,
# the toolchains image a release publishes, built from the repository root), for
# every `COPY`/`ADD` that reads the build context (`--from=` copies read an earlier
# stage, and remote `ADD` sources read the network — neither touches the context):
#   1. the source path exists in the repository, and
#   2. the allowlist that applies to THAT Dockerfile leaves it in the context: the
#      root `.dockerignore`, and a sibling `<Dockerfile>.dockerignore` too where one
#      exists, since BuildKit prefers the sibling and other builders read the root.
# Then that no allowlist admits a path git ignores, that every re-inclusion names a
# path rather than a wildcard family, and THE RUST PIN: a `FROM … rust:<v>` or an
# `ARG RUST_VERSION=<v>` default under `containers/` names rust-toolchain.toml's
# channel, which the kit decides.
#
# The evaluation mirrors Docker's own: patterns apply in order, the last one to match
# wins, a `!` pattern re-includes, and a path is ignored when it or any ancestor
# directory is. `*` and `?` do not cross `/`; `**` does.
#
# Needs no toolchain and no history — it reads the checked-out tree.
#
# Usage:
#   scripts/ci/build-context.sh
set -euo pipefail
# shellcheck source=scripts/ci/tcab-lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/tcab-lib.sh"

# --- the .dockerignore matcher ----------------------------------------------

# Ordered pattern tables, filled by load_dockerignore: one anchored ERE per pattern,
# a parallel flag saying whether the pattern was negated (`!`, i.e. re-includes), and
# the pattern's own text (without the `!`), which the shape check at the foot of this
# file reads because a wildcard is a property of the pattern and not of its regex.
DI_REGEX=()
DI_NEGATED=()
DI_RAW=()

# Translate one .dockerignore pattern into an anchored ERE with Docker's glob
# semantics: `*` and `?` stop at a path separator, `**` spans them, and a leading
# `**/` also matches at the root (so `**/target` covers a top-level `target`).
pattern_to_regex() {
	local pattern="$1" out="" prefix="^" i char
	pattern="${pattern#/}"
	pattern="${pattern%/}"
	if [[ "$pattern" == '**/'* ]]; then
		prefix='^(.*/)?'
		pattern="${pattern#\*\*/}"
	fi
	for ((i = 0; i < ${#pattern}; i++)); do
		char="${pattern:i:1}"
		case "$char" in
		'*')
			if [[ "${pattern:i:2}" == '**' ]]; then
				out+='.*'
				((i++)) || true
			else
				out+='[^/]*'
			fi
			;;
		'?') out+='[^/]' ;;
		# Everything ERE treats as syntax, taken literally.
		'.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | \\) out+="\\$char" ;;
		*) out+="$char" ;;
		esac
	done
	printf '%s%s$' "$prefix" "$out"
}

# Read a .dockerignore into the pattern tables, dropping blanks and comments.
load_dockerignore() {
	local file="$1" line
	DI_REGEX=()
	DI_NEGATED=()
	DI_RAW=()
	while IFS= read -r line || [[ -n "$line" ]]; do
		line="${line%$'\r'}"
		# Trim surrounding whitespace; a pattern never has meaningful edges.
		line="${line#"${line%%[![:space:]]*}"}"
		line="${line%"${line##*[![:space:]]}"}"
		[[ -z "$line" || "$line" == '#'* ]] && continue
		if [[ "$line" == '!'* ]]; then
			DI_NEGATED+=(1)
			DI_RAW+=("${line#!}")
			DI_REGEX+=("$(pattern_to_regex "${line#!}")")
		else
			DI_NEGATED+=(0)
			DI_RAW+=("$line")
			DI_REGEX+=("$(pattern_to_regex "$line")")
		fi
	done <"$file"
}

# True when the path is still in the build context after every pattern has applied.
context_includes() {
	local path="$1" ignored=0 index regex candidate hit
	for index in "${!DI_REGEX[@]}"; do
		regex="${DI_REGEX[$index]}"
		hit=0
		candidate="$path"
		# A path is matched when it matches, or when any ancestor directory does —
		# an ignored directory takes everything under it.
		while :; do
			if [[ "$candidate" =~ $regex ]]; then
				hit=1
				break
			fi
			[[ "$candidate" == */* ]] || break
			candidate="${candidate%/*}"
		done
		if ((hit)); then
			if ((DI_NEGATED[index])); then ignored=0; else ignored=1; fi
		fi
	done
	((ignored == 0))
}

# True when a DIRECTORY source contributes anything at all to the build context.
# `COPY dir dst` copies what survives UNDER the directory, so a directory that the
# allowlist ignores while re-including files beneath it copies those files perfectly
# well — which is what a re-inclusion glob like `!/packages/gg-sandbox-*/*-version.sh`
# is for. Asking `context_includes` about the directory itself would call that a
# failure. Tracked files only (`git ls-files`), because untracked scratch is not a
# thing a build may depend on and walking `target/` here would be absurd.
context_includes_dir() {
	local dir="$1" file
	context_includes "$dir" && return 0
	while IFS= read -r file; do
		context_includes "$file" && return 0
	done < <(git -C "$REPO_ROOT" ls-files -- "$dir")
	return 1
}

# --- the matcher's teeth ----------------------------------------------------

# A gate nobody has watched fail is a gate nobody knows works. This drives the
# matcher over a synthetic allowlist of the shape this repository uses and asserts
# both answers, including the two shapes that have actually broken a build here: a
# file re-included by name, and a file under a re-included directory. The third
# shape — a re-inclusion that is a GLOB spanning one path segment
# (`!/packages/gg-sandbox-*/*-version.sh`, how the devcontainer's allowlist once
# admitted every arm's version file without naming the arms) — is still asserted
# even though no allowlist in the repository may use it any more (the shape check at
# the foot of this file says why): the matcher has to read what a rejected pattern
# WOULD have admitted, and its correctness rests entirely on `*` stopping at a `/`.
self_test() {
	local fixture failures=0 case_line path expected
	fixture="$(mktemp)"
	cat >"$fixture" <<'PATTERNS'
# a comment, and a blank line, both ignored

*
!/crates
!/scripts/ci/install-java.sh
!/apps/web
!/docs/*.md
!/vendor/**/keep.txt
!/packages/gg-sandbox-*/*-version.sh
**/target/
**/node_modules/
PATTERNS
	load_dockerignore "$fixture"
	rm -f "$fixture"

	while read -r path expected; do
		[[ -n "$path" ]] || continue
		if context_includes "$path"; then
			case_line="included"
		else
			case_line="ignored"
		fi
		if [[ "$case_line" != "$expected" ]]; then
			echo "error: matcher self-test: '$path' came back $case_line, expected $expected" >&2
			failures=$((failures + 1))
		fi
	done <<'CASES'
crates/gg/src/main.rs included
crates/Cargo.toml included
scripts/ci/install-java.sh included
scripts/ci/install-purescript.sh ignored
scripts/ci ignored
packages/gg-sandbox-java/java-version.sh included
packages/gg-sandbox-java/src/Main.java ignored
packages/gg-sandbox-swift/.build/debug/swift-version.sh ignored
packages/gg-sandbox-java ignored
packages/ui/src/index.ts ignored
apps/web/src/main.tsx included
apps/docs/astro.config.mjs ignored
crates/gg/target/debug/gg ignored
crates/target ignored
target ignored
apps/web/node_modules/left-pad/index.js ignored
Cargo.toml ignored
docs/overview.md included
docs/nested/overview.md ignored
vendor/one/two/keep.txt included
vendor/one/two/drop.txt ignored
CASES

	((failures == 0)) || {
		echo "error: the .dockerignore matcher is wrong; its verdicts below cannot be trusted." >&2
		exit 1
	}
}

# --- reading the Dockerfiles ------------------------------------------------

# Print `<line-number> <instruction and its arguments, one logical line>` for every
# COPY/ADD in the file, joining backslash continuations and dropping comment lines.
copy_instructions() {
	awk '
		{
			line = $0
			sub(/\r$/, "", line)
			if (pending != "") {
				# Inside a continuation: a comment line is a comment, not an argument.
				if (line ~ /^[[:space:]]*#/) next
				sub(/^[[:space:]]+/, "", line)
				if (line ~ /\\[[:space:]]*$/) {
					sub(/\\[[:space:]]*$/, "", line)
					pending = pending " " line
					next
				}
				print start, pending " " line
				pending = ""
				next
			}
			if (line ~ /^[[:space:]]*#/) next
			if (line !~ /^[[:space:]]*(COPY|ADD)[[:space:]]/) next
			sub(/^[[:space:]]+/, "", line)
			if (line ~ /\\[[:space:]]*$/) {
				sub(/\\[[:space:]]*$/, "", line)
				pending = line
				start = NR
				next
			}
			print NR, line
		}
	' "$1"
}

log "verify every Dockerfile COPY can see its source in the build context"
self_test

problems=0
checked=0
mapfile -t dockerfiles < <(git ls-files '*Dockerfile' '*.Dockerfile' '*.dockerfile' | sort)
((${#dockerfiles[@]} > 0)) || {
	echo "error: no tracked Dockerfiles found; this gate would pass vacuously." >&2
	exit 1
}

# Check one Dockerfile's context-reading COPY/ADD sources against the allowlist that
# is currently loaded. Both are named in every message: which Dockerfile, and which
# allowlist rejected the source — the caller runs this once per allowlist that can
# apply to that Dockerfile, so "which one" is the actionable half.
check_dockerfile() {
	local dockerfile="$1" ignore_file="$2"
	local lineno instruction arg source normalized from_stage
	local -a args positional

	while read -r lineno instruction; do
		[[ -n "$instruction" ]] || continue
		# shellcheck disable=SC2206  # deliberate word-splitting: Dockerfile arguments
		args=($instruction)
		# Drop the instruction keyword and every flag; keep the positional arguments.
		positional=()
		from_stage=""
		for arg in "${args[@]:1}"; do
			case "$arg" in
			--from=*)
				from_stage="${arg#--from=}"
				;;
			--*) ;;
			*) positional+=("$arg") ;;
			esac
		done
		# `--from=` reads an earlier stage or an external image, never the context.
		[[ -n "$from_stage" ]] && continue
		# The last positional argument is the destination.
		((${#positional[@]} >= 2)) || continue
		for source in "${positional[@]:0:${#positional[@]}-1}"; do
			# A remote ADD reads the network; a whole-context copy takes whatever the
			# allowlist admits, which is the allowlist's own answer.
			[[ "$source" == *://* ]] && continue
			[[ "$source" == '.' || "$source" == './' ]] && continue
			# A source built from a build arg (the `ADD --checksum=… ${PACK_URL}` pack
			# fetches) resolves at build time, not here; there is no path to check.
			[[ "$source" == *'$'* ]] && continue
			# A glob's expansion is the build context's business, not ours.
			[[ "$source" == *'*'* || "$source" == *'?'* || "$source" == *'['* ]] && continue
			normalized="${source#./}"
			checked=$((checked + 1))
			if [[ ! -e "$REPO_ROOT/$normalized" ]]; then
				echo "error: $dockerfile:$lineno copies '$source', which does not exist in the repository." >&2
				problems=$((problems + 1))
				continue
			fi
			# A directory source copies its surviving CONTENTS, so the question for it is
			# whether anything under it survives — see context_includes_dir.
			if [[ -d "$REPO_ROOT/$normalized" ]]; then
				if ! context_includes_dir "$normalized"; then
					echo "error: $dockerfile:$lineno copies the directory '$source', and $ignore_file keeps every tracked file under it OUT of the build context." >&2
					echo "       The build will fail with: failed to compute cache key: \"/$normalized\": not found" >&2
					echo "       Fix: re-include what that image reads in $ignore_file, with a comment saying which image needs it." >&2
					problems=$((problems + 1))
				fi
			elif ! context_includes "$normalized"; then
				echo "error: $dockerfile:$lineno copies '$source', which $ignore_file keeps OUT of the build context." >&2
				echo "       The build will fail with: failed to compute cache key: \"/$normalized\": not found" >&2
				echo "       Fix: add '!/$normalized' to the $ignore_file allowlist, with a comment saying which image needs it." >&2
				problems=$((problems + 1))
			fi
		done
	done < <(copy_instructions "$dockerfile")
}

# Pass one: the root allowlist, which can apply to every Dockerfile here, being the only
# ignore file a builder that reads no sibling file reaches. Loaded once, being the same file
# for all.
load_dockerignore "$REPO_ROOT/.dockerignore"
for dockerfile in "${dockerfiles[@]}"; do
	check_dockerfile "$dockerfile" ".dockerignore"
done

# Pass two: the sibling allowlists, which BuildKit prefers for the Dockerfiles that
# have one. A separate pass rather than a per-Dockerfile switch so that the root file
# is parsed once rather than once per alternation.
for dockerfile in "${dockerfiles[@]}"; do
	sibling="$dockerfile.dockerignore"
	[[ -f "$REPO_ROOT/$sibling" ]] || continue
	load_dockerignore "$REPO_ROOT/$sibling"
	check_dockerfile "$dockerfile" "$sibling"
done

# --- what an allowlist lets in by accident -----------------------------------

# Everything above asks "does the context still contain what a build READS". This asks
# the other question, which nothing here used to ask: does it contain anything a build
# does NOT read. An allowlist makes that failure silent — `!/crates` re-includes a
# directory, not a file list, so every future untracked or generated file under it joins
# the context of every whole-context build without a line changing anywhere.
#
# THE COST IS NOT DISK, IT IS THE CACHE. A directory COPY's cache key covers what it copies,
# so a file no build reads still invalidates every layer after it when it changes, and the
# layers after a toolchain image's COPY are downloads of gigabytes.
#
# Machine-local output under a re-included tree (fetched binaries, `.DS_Store` files,
# generated schema trees, a `tsconfig.tsbuildinfo`) rides in exactly this way, and no gate
# that reads COPY sources can see it.
#
# THE RULE THIS ASSERTS: a path git ignores is not a build input. Ignored means generated,
# fetched, machine-local or scratch — every one of which is either reproduced inside the
# image or has no business in it — so the context should be exactly the tracked tree. It
# was, on the tree this check landed with, and that is a much easier invariant to keep
# than a list of the shapes that have leaked so far.
#
# UNTRACKED-BUT-NOT-IGNORED FILES ARE DELIBERATELY ALLOWED: a source file added and not yet
# committed is a real build input, and a gate that ran before `git add` and rejected it
# would be wrong.
#
# Checked against EVERY allowlist that can apply, for the reason pass two above exists: a
# sibling allowlist that re-includes a directory whole is exactly the shape that lets
# ignored scratch in.
#
# `--directory` collapses a wholly-ignored directory to one entry, so this neither walks
# `target/` nor reports ten thousand paths inside it; it is a few hundred entries on a
# working tree and a handful on a fresh clone. A fresh clone is also why the count is
# printed rather than asserted non-zero: on a runner that has built nothing there is
# genuinely nothing to check, and a vacuous pass should be visible rather than fatal.
mapfile -t ignored_paths < <(git -C "$REPO_ROOT" ls-files -o -i --exclude-standard --directory)

# Every allowlist in the repository: the root one, plus each sibling that exists.
ignore_files=(".dockerignore")
for dockerfile in "${dockerfiles[@]}"; do
	[[ -f "$REPO_ROOT/$dockerfile.dockerignore" ]] && ignore_files+=("$dockerfile.dockerignore")
done

ignored_checked=0
for ignore_file in "${ignore_files[@]}"; do
	load_dockerignore "$REPO_ROOT/$ignore_file"
	for path in "${ignored_paths[@]}"; do
		path="${path%/}"
		[[ -n "$path" ]] || continue
		ignored_checked=$((ignored_checked + 1))
		context_includes "$path" || continue
		echo "error: $ignore_file lets '$path' into the build context, and git ignores it." >&2
		echo "       Ignored means generated, fetched or machine-local, so no image reads it — but it" >&2
		echo "       still joins the COPY cache key, and rebuilds the Rust workspace whenever it changes." >&2
		echo "       Fix: re-exclude it at the FOOT of $ignore_file (last match wins), with a comment." >&2
		problems=$((problems + 1))
	done
done

# --- what an allowlist's SHAPE costs, before a byte is transferred ------------

# Every check above is about WHICH paths an allowlist admits. This one is about how the
# allowlist is written, because one shape of pattern changes what the builder does with the
# paths it does NOT admit. BuildKit's context sender skips an ignored directory without
# entering it only while it can prove no negated pattern is rooted beneath it, and its
# proof is a string-prefix test over the negations. A negation that contains a wildcard
# (`*`, `?` or `[`) cannot be prefix-tested, so ONE such line anywhere in the file turns
# the optimisation off for EVERY directory: the sender then walks the whole working tree —
# node_modules, target/, tmp/, ~650,000 entries on a developer machine — to ship the few
# hundred kB it keeps. That is the difference between a two-second and a twenty-second
# context transfer on every image build, and it is the difference between a build that
# never touches tmp/ and one that dies because a single entry under it could not be
# lstat'ed, which has happened in the superrepo, whose gate this one is cut from.
#
# THE RULE: a re-inclusion names a path, never a family. Excludes may still be globs —
# `**/node_modules/` is an exclude, and the sender handles those without walking.
shape_checked=0
for ignore_file in "${ignore_files[@]}"; do
	load_dockerignore "$REPO_ROOT/$ignore_file"
	for index in "${!DI_RAW[@]}"; do
		((DI_NEGATED[index] == 1)) || continue
		shape_checked=$((shape_checked + 1))
		[[ "${DI_RAW[$index]}" == *[\*\?\[]* ]] || continue
		echo "error: $ignore_file re-includes '!${DI_RAW[$index]}' with a wildcard, and a re-inclusion must name a path." >&2
		echo "       BuildKit skips an ignored directory only while every negation can be prefix-tested against it;" >&2
		echo "       one wildcard negation makes the context sender walk the whole working tree on every build." >&2
		echo "       Fix: replace the glob with one '!' line per file, and list the family below so the gate keeps it complete." >&2
		problems=$((problems + 1))
	done
done

# --- the Rust the case images pin, against the Rust the template decides -------

# See THE RUST PIN in the header: every `FROM … rust:<v>-…` and every
# `ARG RUST_VERSION=<v>` default under `containers/` must name the toolchain's
# channel. An `ARG RUST_VERSION` without a default takes its value from the build,
# and is left alone; that is the toolchains image's own, which its build is handed
# from packages/gg-sandbox-rust/rust-version.sh, so it may find none.
rust_channel="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
[[ -n "$rust_channel" ]] || {
	echo "error: rust-toolchain.toml names no channel, so the case images' Rust pins cannot be checked." >&2
	exit 1
}
rust_pins_checked=0
while IFS= read -r pin; do
	pin_file="${pin%%:*}"
	pin="${pin#*:}"
	pin_line="${pin%%:*}"
	pin_text="${pin#*:}"
	if [[ "$pin_text" =~ ^[[:space:]]*ARG[[:space:]]+RUST_VERSION=([^[:space:]]+) ]]; then
		pin_version="${BASH_REMATCH[1]}"
	elif [[ "$pin_text" =~ (^|[/[:space:]])rust:([^-[:space:]@]+) ]]; then
		pin_version="${BASH_REMATCH[2]}"
	else
		continue
	fi
	rust_pins_checked=$((rust_pins_checked + 1))
	[[ "$pin_version" == "$rust_channel" ]] && continue
	echo "error: $pin_file:$pin_line pins Rust $pin_version, and rust-toolchain.toml's channel is $rust_channel." >&2
	echo "       The kit decides the compiler; an image built with another one is untested." >&2
	echo "       Fix: set the pin to $rust_channel." >&2
	problems=$((problems + 1))
done < <(
	git -C "$REPO_ROOT" ls-files -z -- 'containers/**Dockerfile' 'containers/**.Dockerfile' 'containers/**.dockerfile' |
		xargs -0 -r -I{} grep -HnE '^[[:space:]]*(FROM[[:space:]].*[/[:space:]]rust:|ARG[[:space:]]+RUST_VERSION=)' "$REPO_ROOT/{}" |
		sed "s#^$REPO_ROOT/##"
)

((problems == 0)) || {
	echo >&2
	echo "$problems build-context problem(s) found across ${#dockerfiles[@]} Dockerfile(s), ${#ignored_paths[@]} git-ignored path(s), $shape_checked re-inclusion(s) and $rust_pins_checked Rust pin(s)." >&2
	exit 1
}

# A count of CHECKS rather than of distinct paths: a Dockerfile with a sibling
# allowlist has its sources checked against both, which is the point.
echo "$checked context-source check(s) — every COPY across ${#dockerfiles[@]} Dockerfile(s), against each allowlist that can apply to it — all survive."
echo "$ignored_checked exclusion check(s) — ${#ignored_paths[@]} git-ignored path(s) against each allowlist — none reach the build context."
echo "$shape_checked re-inclusion(s) across ${#ignore_files[@]} allowlist(s) name a path rather than a wildcard family."
echo "$rust_pins_checked Rust pin(s) under containers/ name rust-toolchain.toml's channel, $rust_channel."
