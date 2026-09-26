import { describe, expect, it } from "vitest";
import { SlskConfSchema, insideMediaRoot } from "./slsk.schemas.ts";

const base = { mediaRoot: "/mnt/kontent", library: "/mnt/kontent/music", downloads: "/mnt/kontent/slsk" };

describe("SlskConfSchema", () => {
  it("fills defaults", () => {
    const c = SlskConfSchema.parse(base);
    expect(c.listenPort).toBe(2240);
    expect(c.statePath).toBe("/var/lib/slsk");
    expect(insideMediaRoot(c)).toBe(true);
  });

  it("refuses relative paths, which would mount the wrong thing silently", () => {
    expect(() => SlskConfSchema.parse({ ...base, library: "music" })).toThrow();
  });

  it("knows when a path is outside the one volume it mounts", () => {
    expect(insideMediaRoot(SlskConfSchema.parse({ ...base, downloads: "/srv/dl" }))).toBe(false);
    expect(insideMediaRoot(SlskConfSchema.parse({ ...base, library: "/mnt/kontentx/music" }))).toBe(false);
  });

  it("refuses privileged ports and unknown keys", () => {
    expect(() => SlskConfSchema.parse({ ...base, listenPort: 80 })).toThrow();
    expect(() => SlskConfSchema.parse({ ...base, roots: [] })).toThrow();
  });
});
