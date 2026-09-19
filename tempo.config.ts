import { defineConfig, presets, task } from "@xevion/tempo";

export default defineConfig({
  tasks: [
    ...presets.rust({
      name: "ferrite",
      override: {
        // Match CI: --all-targets --all-features -D warnings
        lint: "cargo clippy --all-targets --all-features -- -D warnings",
        // Match CI: --no-fail-fast --hide-progress-bar --failure-output final
        test: "cargo nextest run --no-fail-fast --hide-progress-bar --failure-output final",
      },
    }),

    // Feature-combination checks: catch compilation failures behind feature gates.
    task({
      name: "ferrite:lint-no-default",
      body: "cargo clippy --all-targets --no-default-features -- -D warnings",
      tags: ["check", "lint"],
    }),
    task({
      name: "ferrite:test-no-default",
      body: "cargo nextest run --no-fail-fast --hide-progress-bar --failure-output final --no-default-features",
      tags: ["check", "test"],
    }),

    // Catches broken intra-doc links, bad code blocks, and bare URLs (denied in lib.rs).
    // No -D warnings here: missing_docs is warn-level by design, not yet a full backfill.
    task({
      name: "ferrite:doc",
      body: "cargo doc --no-deps --all-features --quiet",
      tags: ["check"],
    }),

    task({
      name: "ferrite:dep-check",
      body: "cargo machete",
      tags: ["check"],
      requires: [{ tool: "cargo-machete", hint: "cargo install cargo-machete" }],
    }),

    task({
      name: "security:audit",
      body: "cargo deny check advisories bans sources",
      tags: ["check"],
      requires: [{ tool: "cargo-deny", hint: "cargo install cargo-deny" }],
    }),
  ],

  commands: {
    check: { description: "Run every check", tags: ["check"] },
    fmt: { description: "Format all sources", tags: ["format"], concurrency: 1 },
    lint: { description: "Clippy only", tags: ["lint"] },
  },
});
