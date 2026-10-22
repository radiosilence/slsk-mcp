# slsk-mcp — a Soulseek client that runs as a service

# What CI checks: format, lint with warnings as errors, tests, the Pulumi package.
check:
    actionlint
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked
    cd deploy/pulumi && npm run -s check

# The checks that take seconds, run before every push; CI runs the rest.
fast:
    git diff --quiet Cargo.lock || { echo 'Cargo.lock has uncommitted changes'; exit 1; }
    cargo fmt --all -- --check

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
