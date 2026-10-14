# One entry point: the git hook, CI and AGENTS.md all run `just check`.

shell_scripts := "assets/app-icon/render.sh packaging/linux/install.sh packaging/macos/bundle.sh .githooks/pre-commit plugin/hooks/handoff-files.sh"

# Everything CI gates on: format, lint, dependency policy, workflows, tests.
check: fmt-check lint deny workflows test

# Fast subset for the pre-commit hook (under a few seconds; no build).
pre-commit: fmt-check typos secrets-staged

fmt:
    cargo fmt
    shfmt -w {{ shell_scripts }}

fmt-check:
    cargo fmt --check
    shfmt -d {{ shell_scripts }}

lint: typos
    cargo clippy --locked --all-targets --all-features -- -D warnings
    shellcheck {{ shell_scripts }}

typos:
    typos

# RustSec advisories, licenses, banned and duplicate crates (deny.toml).
deny:
    cargo deny --locked check

# Workflow syntax (actionlint) and security (zizmor).
workflows:
    actionlint
    zizmor --offline .github/workflows

test:
    cargo test --locked

# Secrets in the whole git history (CI) and in the staged changes (hook).
secrets:
    gitleaks git --redact --no-banner

secrets-staged:
    gitleaks git --staged --redact --no-banner

# Install the pinned tools and the git hook.
setup:
    mise install
    git config core.hooksPath .githooks

# Pushes main and the tag together, so the tag is not left behind as with a plain `git push`.
# Publish a release: bump the version, run the gate, commit, tag and push (`just release 0.3.2`).
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    new="{{ version }}"
    [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: just release X.Y.Z" >&2; exit 1; }
    [[ "$(git branch --show-current)" == main ]] || { echo "not on main" >&2; exit 1; }
    git diff --quiet HEAD || { echo "uncommitted changes" >&2; exit 1; }
    git fetch --quiet origin main --tags
    [[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || { echo "main differs from origin/main" >&2; exit 1; }
    git rev-parse -q --verify "refs/tags/v$new" >/dev/null && { echo "tag v$new exists" >&2; exit 1; }
    old=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
    sed -i "0,/^version = \"$old\"/s//version = \"$new\"/" Cargo.toml
    sed -i "s/\"version\": \"$old\"/\"version\": \"$new\"/" plugin/.claude-plugin/plugin.json
    cargo update --offline -p scrpad
    just check
    git add Cargo.toml Cargo.lock plugin/.claude-plugin/plugin.json
    git commit -m "Release $new"
    git tag "v$new"
    git push origin main "v$new"
