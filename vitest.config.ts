// The workspace's tests: every package's own suite, each run as a vitest
// project, so one run from the root covers them all and writes one report.
// `@clockwyrks/gg-sandbox` is the one package with a suite: the helpers of the
// TypeScript guest's SDK that touch no membrane import, run under Node. The
// rest of gg is tested by its Rust suite (scripts/ci/gg-test.sh), which drives
// every arm's guest through gg itself.
//
// The projects are named rather than globbed, so a directory under packages/
// that is not a package is no project.
//
// Given CI_GATE_ARTIFACTS (the typescript gate sets it), the run writes the JUnit
// report and the coverage summary into that directory, as ci/README.md names
// them; without it, it writes neither.
import { defineConfig } from "vitest/config";

const artifacts = process.env.CI_GATE_ARTIFACTS;

export default defineConfig({
  test: {
    projects: [
      {
        test: {
          name: "@clockwyrks/gg-sandbox",
          root: "./packages/gg-sandbox",
          include: ["test/**/*.test.ts"],
          environment: "node",
        },
      },
    ],
    ...(artifacts
      ? {
          reporters: ["default", "junit"],
          outputFile: { junit: `${artifacts}/junit.xml` },
          coverage: {
            enabled: true,
            provider: "v8",
            reporter: ["json-summary"],
            reportsDirectory: artifacts,
            include: ["packages/*/src/**/*.{ts,js}"],
          },
        }
      : {}),
  },
});
