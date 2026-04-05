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
        "dep-check": {
          cmd: "cargo machete",
          hint: "Remove the unused dependency from Cargo.toml",
          requires: ["cargo-machete"],
        },
      },
    },
    security: {
      alwaysRun: true,
      aliases: ["sec", "audit"],
      commands: {
        audit: {
          cmd: "cargo deny check advisories sources",
          requires: ["cargo-deny"],
          hint: "Install cargo-deny: cargo install cargo-deny",
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
