import { describe, expect, it } from "vitest";
import { SlskConfSchema } from "./slsk.schemas.ts";

describe("SlskConfSchema", () => {
  it("fills defaults from the library alone", () => {
    const c = SlskConfSchema.parse({ library: "/mnt/kontent/music" });
    expect(c.listenPort).toBe(2240);
    expect(c.statePath).toBe("/var/lib/slsk");
    expect(c.postgres.limits.memory).toBe("256Mi");
  });

  it("refuses a relative library, which would mount the wrong thing silently", () => {
    expect(() => SlskConfSchema.parse({ library: "music" })).toThrow();
  });

  it("refuses the port every other client defaults to below 1024 and unknown keys", () => {
    expect(() => SlskConfSchema.parse({ library: "/m", listenPort: 80 })).toThrow();
    expect(() => SlskConfSchema.parse({ library: "/m", roots: [] })).toThrow();
  });
});
