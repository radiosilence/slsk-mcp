# slsk-mcp — a Soulseek client that runs as a service

# What CI checks, short of the audit and the release builds: format, lint with
# warnings as errors, tests, the Pulumi package, the compiled stylesheet, the
# changelog fragments and the one version number.
check:
    actionlint
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check
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

# Fail when the committed stylesheet is not what its sources compile to.
css-fresh:
    #!/usr/bin/env bash
    set -euo pipefail
    css=$(mktemp)
    trap 'rm -f "$css"' EXIT
    tailwindcss --optimize --input crates/slsk-mcp/styles/app.css --output "$css"
    cmp -s "$css" crates/slsk-mcp/assets/app.css || { echo 'assets/app.css is stale: run `just css`'; exit 1; }

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

# Compile the web UI's stylesheet from crates/slsk-mcp/styles/. The output is committed because
# the server embeds it; CI fails a build where it is stale.
css:
    tailwindcss --optimize --input crates/slsk-mcp/styles/app.css --output crates/slsk-mcp/assets/app.css

# Format
fmt:
    cargo fmt --all

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
