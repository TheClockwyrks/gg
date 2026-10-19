"""Hold every Dockerfile's COPY sources to the build context it is given.

The root `.dockerignore` is an allowlist, so a COPY of a path it keeps out fails
only when the image is next built, which for gg's toolchains image is a release
tag, long after the commit. scripts/ci/build-context.sh reads each Dockerfile
against the allowlists that can apply to it, and holds each Rust pin under
`containers/` to `rust-toolchain.toml`.

The gate is this repository's own, not the kit's: the pipeline renders a step
for it from the kit's table of extra gates, and an update of the kit never
touches this file.
"""

from the_test_cabinet_ci import GIT_LOCATION_VARIABLES, enter_repo_root, fail, run

enter_repo_root()

# The script lists tracked files with `git ls-files`, so it must not inherit a
# commit hook's GIT_DIR (see GIT_LOCATION_VARIABLES).
if run(["scripts/ci/build-context.sh"], unset=GIT_LOCATION_VARIABLES).returncode != 0:
    fail(
        "",
        "A Dockerfile copies what its build context leaves out, or a Rust pin under",
        "containers/ differs from rust-toolchain.toml. The errors above name the file.",
    )
