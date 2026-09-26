import * as z from "zod";
import { AbsolutePath, ResourcesSchema } from "./contract.ts";

export const SlskConfSchema = z.strictObject({
  /**
   * The music library: where imported albums are filed, and what is shared
   * unless `shares` says otherwise. A local volume on `node`, at this path.
   */
  library: AbsolutePath,
  /**
   * `incomplete/` for downloads in progress, `complete/` for finished ones
   * waiting for import or for a person. Best on the library's drive, where
   * an import is a rename.
   */
  downloads: AbsolutePath,
  /** Shared with the network, read-only. Defaults to the library. */
  shares: z.array(AbsolutePath).optional(),
  /** Nominal; a local volume has whatever the drive has. */
  capacity: z.string().default("2Ti"),
  /**
   * The service's own state on the node's internal disk: Postgres and the
   * share-probe cache. Not the media drive —
   * this is the machine's state, and should not go missing when the drive
   * does. Created and handed to uid 1000 by an init container.
   */
  statePath: AbsolutePath.default("/var/lib/slsk"),
  /**
   * The Soulseek peer port, published on the node. Not 2234, the default
   * every other client uses: a laptop running one on the same network asks
   * the router for the same forward, and the two would take turns owning it.
   */
  listenPort: z.number().int().min(1024).max(65535).default(2240),
  /** Keep a UPnP forward for `listenPort` on the router. */
  upnp: z.boolean().default(true),
  uploadSlots: z.number().int().positive().default(5),
  /** Bytes per second, 0 for unlimited. */
  uploadLimit: z.number().int().nonnegative().default(0),
  downloadLimit: z.number().int().nonnegative().default(0),
  /**
   * A beets `config.yaml`, read by the importer for its path template,
   * replacements and matching threshold. `directory` in it is ignored; the
   * library is `library`.
   */
  beetsConfig: z.string().optional(),
  /** Shown to peers who ask for our user info. */
  description: z.string().default(""),
  limits: ResourcesSchema.default({ cpu: "2", memory: "1Gi" }),
  requests: ResourcesSchema.optional(),
  postgres: z
    .strictObject({
      limits: ResourcesSchema.default({ cpu: "500m", memory: "256Mi" }),
      requests: ResourcesSchema.optional(),
    })
    .prefault({}),
});
