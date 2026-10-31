/**
 * The version this chart deploys: the crate's version, and the package's.
 * slsk-mcp and the chart that deploys it ship together, so one number says
 * exactly which build a pin gets. CI refuses a release where `Cargo.toml`,
 * `package.json` and this disagree.
 */
export const APP_VERSION = "0.9.2";

export const VERSIONS = {
  slsk: `ghcr.io/radiosilence/slsk-mcp:v${APP_VERSION}`,
  alpine: "alpine:3.21@sha256:ee16d92f96db08d9d2d07da48555a27f3a69d8f0759021dbc601195ed0d2b5bd",
} as const;
