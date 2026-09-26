import { describe, expect, it } from "vitest";
import { SlskConfSchema } from "./slsk.schemas.ts";

describe("SlskConfSchema", () => {
  it("fills defaults from the library alone", () => {
    const c = SlskConfSchema.parse({ library: "/mnt/kontent/music", downloads: "/mnt/kontent/slsk" });
    expect(c.listenPort).toBe(2240);
    expect(c.statePath).toBe("/var/lib/slsk");
    expect(c.postgres.limits.memory).toBe("256Mi");
  });

  it("refuses relative paths, which would mount the wrong thing silently", () => {
    expect(() => SlskConfSchema.parse({ library: "music", downloads: "/d" })).toThrow();
    expect(() => SlskConfSchema.parse({ library: "/m", downloads: "slsk" })).toThrow();
  });

  it("refuses the port every other client defaults to below 1024 and unknown keys", () => {
    const base = { library: "/m", downloads: "/d" };
    expect(() => SlskConfSchema.parse({ ...base, listenPort: 80 })).toThrow();
    expect(() => SlskConfSchema.parse({ ...base, roots: [] })).toThrow();
  });
});
