# slsk-mcp — a Soulseek client that runs as a service

# What CI checks, short of the audit and the release builds: format (Rust and
# templates), lint with warnings as errors, tests, the Pulumi package, the
# compiled stylesheet, the changelog fragments and the one version number.
check:
    actionlint
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check
    askama-analyzer fmt --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked
    cd deploy/pulumi && npm run -s check && npm run -s build
    just css-fresh
    python3 -m unittest scripts/test_changelog.py
    python3 scripts/changelog.py --lint
    just versions

# The checks that take seconds, run before every push; CI runs the rest.
fast:
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check
    askama-analyzer fmt --check

# Fail when the committed stylesheet is not what the sources and the kit give.
css-fresh: kit
    #!/usr/bin/env bash
    set -euo pipefail
    css=$(mktemp)
    trap 'rm -f "$css"' EXIT
    tailwindcss --optimize --input crates/slsk-mcp/styles/app.css --output "$css"
    cmp -s "$css" crates/slsk-mcp/assets/app.css || { echo 'assets/app.css is stale: run `just css`'; exit 1; }
    node crates/slsk-mcp/styles/kit/kit-check.mjs crates/slsk-mcp/templates crates/slsk-mcp/src

# Point crates/slsk-mcp/styles/kit at koan-kit-web's source, the version Cargo.lock names, so the stylesheet is compiled
# from the same kit release as the markup its components write. The link is gitignored.
kit:
    #!/usr/bin/env bash
    set -euo pipefail
    manifest=$(cargo metadata --format-version 1 --locked | python3 -c 'import json, sys; print(next(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"] == "koan-kit-web"))')
    ln -sfn "$(dirname "$manifest")" crates/slsk-mcp/styles/kit

# Fail on markup outside the kōan web kit: rounded corners, shadows, Tailwind's palette and type sizes, inline styles.
# Most compile to nothing, so a leftover silently loses its look. A deliberate exception carries `kit: raw` and why.
kit-check: kit
    node crates/slsk-mcp/styles/kit/kit-check.mjs crates/slsk-mcp/templates crates/slsk-mcp/src

# Fail unless the crate, the Pulumi package and the image it deploys share one version.
versions:
    #!/usr/bin/env bash
    set -euo pipefail
    crate=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/slsk-mcp/Cargo.toml | head -1)
    chart=$(node -p "require('./deploy/pulumi/package.json').version")
    app=$(sed -n 's/^export const APP_VERSION = "\(.*\)";/\1/p' deploy/pulumi/src/versions.ts)
    if [ "$crate" != "$chart" ] || [ "$crate" != "$app" ]; then
        echo "versions disagree: Cargo.toml $crate, package.json $chart, versions.ts $app"
        exit 1
    fi

# Compile the web UI's stylesheet from crates/slsk-mcp/styles/ and the kit's source. Committed because the server
# embeds it; the server serves the kit's typeface from the crate. CI fails a build where the stylesheet is stale.
css: kit-check
    tailwindcss --optimize --input crates/slsk-mcp/styles/app.css --output crates/slsk-mcp/assets/app.css

# Run the service on http://127.0.0.1:8090 (or $DEV_PORT) against a Soulseek network on this machine, with dummy albums in the
# library and on one sharing peer, rebuilding as crates/slsk-mcp changes. State lives in .dev/; delete it to start afresh.
dev:
    scripts/dev.sh

# Format the Rust and the templates
fmt:
    cargo fmt --all
    askama-analyzer fmt

# Install the git hooks
hooks:
    lefthook install

# Write CHANGELOG.md from the fragments in changelog.d/. `--stdout` prints it instead.
changelog *args:
    python3 scripts/changelog.py {{args}}

# Open the release PR: `just release patch|minor|major`, or an exact version.
# Merging it publishes the image, the Pulumi package and the GitHub release; tags are never pushed by hand.
release version *args:
    scripts/release.sh {{version}} {{args}}
