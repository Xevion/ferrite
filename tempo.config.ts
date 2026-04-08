import { defineConfig, presets, runners } from "@xevion/tempo";

const ferritePreset = presets.rust();

export default defineConfig({
  subsystems: {
    ferrite: {
      ...ferritePreset,
      aliases: ["f"],
      commands: {
        ...ferritePreset.commands,
        // Match CI: --all-targets --all-features -D warnings
        lint: "cargo clippy --all-targets --all-features -- -D warnings",
        // Match CI: --no-fail-fast --hide-progress-bar --failure-output final
        test: "cargo nextest run --no-fail-fast --hide-progress-bar --failure-output final",
        // Feature-combination checks: catch compilation failures behind feature gates
        "lint-no-default": "cargo clippy --all-targets --no-default-features -- -D warnings",
        "test-no-default":
          "cargo nextest run --no-fail-fast --hide-progress-bar --failure-output final --no-default-features",
        "dep-check": {
          cmd: "cargo machete",
          requires: [{ tool: "cargo-machete", hint: "Install with `cargo install cargo-machete`" }],
        },
      },
    },
    security: {
      alwaysRun: true,
      aliases: ["sec", "audit"],
      commands: {
        audit: {
          cmd: "cargo deny check advisories bans sources",
          requires: [{ tool: "cargo-deny", hint: "Install with `cargo install cargo-deny`" }],
        },
      },
    },
  },
  commands: {
    check: runners.check({
      autoFixStrategy: "fix-first",
      exclude: ["ferrite:build"],
    }),
    fmt: runners.sequential("format-apply", {
      description: "Format all subsystems",
      autoFixFallback: true,
    }),
    lint: runners.sequential("lint", {
      description: "Lint all subsystems",
    }),
    "pre-commit": runners.preCommit(),
  },
  ci: {
    groupedOutput: true,
  },
});
