/**
 * The version this chart deploys: the crate's version, and the package's.
 * slsk-mcp and the chart that deploys it ship together, so one number says
 * exactly which build a pin gets. CI refuses a release where `Cargo.toml`,
 * `package.json` and this disagree.
 */
export const APP_VERSION = "0.1.48";

export const VERSIONS = {
  slsk: `ghcr.io/radiosilence/slsk-mcp:v${APP_VERSION}`,
  postgres: "postgres:18.1-alpine",
  alpine: "alpine:3.21",
} as const;
