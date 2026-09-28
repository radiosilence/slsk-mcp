import * as k8s from "@pulumi/kubernetes";
import * as pulumi from "@pulumi/pulumi";
import type * as z from "zod";
import type { Deployed } from "./contract.ts";
import { SlskConfSchema, insideMediaRoot } from "./slsk.schemas.ts";
import { VERSIONS } from "./versions.ts";

const NAME = "slsk";
const SECRETS_NAME = "slsk-secrets";
const UI_PORT = 8080;
/**
 * MCP and GraphQL. Credentials arriving in headers on this port are trusted,
 * so the NetworkPolicy admits the gateway and nothing else.
 */
export const INTERNAL_PORT = 8081;
/** `/metrics` alone, so the scraper is let in without reaching the above. */
export const METRICS_PORT = 9464;

const secretRef = (key: string) => ({
  valueFrom: { secretKeyRef: { name: SECRETS_NAME, key } },
});

/** Keeps a UPnP forward for the peer port on the router. See mariastew's. */
const PORTMAP = `set -eu
apk add --no-cache miniupnpc >/dev/null
lan() { ip -4 route get 1.1.1.1 | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}'; }
while :; do
  ip=$(lan)
  if upnpc -e slsk -a "$ip" "$PORT" "$PORT" TCP "$LEASE" >/dev/null 2>&1; then
    echo "mapped TCP $PORT -> $ip:$PORT for $LEASE""s"
  else
    echo "WARNING: could not map TCP $PORT; peers behind their own NAT cannot reach us"
  fi
  sleep $((LEASE / 2))
done`;

/** Private and link-local space: everything the pod has no business reaching. */
const PRIVATE = [
  "10.0.0.0/8",
  "172.16.0.0/12",
  "192.168.0.0/16",
  "169.254.0.0/16",
  "100.64.0.0/10",
];

/**
 * slsk-mcp: a Soulseek client, the Postgres it keeps jobs in, and a UPnP
 * mapper for its peer port.
 *
 * Postgres is a container in the same pod rather than a deployment of its
 * own. It holds one service's jobs and sealed credentials, listens on the
 * pod's loopback only, and lives and dies with the pod on the node whose disk
 * holds its data — a separate deployment would add a Service, a policy and a
 * scheduling constraint to reach the same place.
 */
export function createSlsk(
  provider: k8s.Provider,
  namespace: pulumi.Input<string>,
  confArgs: z.input<typeof SlskConfSchema>,
  opts: {
    hostname: string;
    /**
     * The node holding the media drive, by hostname: a local volume's
     * affinity names a node, and the pod follows the volume there.
     */
    node: string;
    oidc: { issuer: string; clientId: string };
    oidcClientSecret: pulumi.Input<string>;
    /** 32 random bytes, base64. Seals Soulseek credentials at rest. */
    sealKey: pulumi.Input<string>;
    databasePassword: pulumi.Input<string>;
    /**
     * An account to log in with at start, so the client is sharing before
     * anyone asks it anything. Credentials from the gateway or the UI replace
     * it.
     */
    account?: { username: pulumi.Input<string>; password: pulumi.Input<string> };
    /**
     * A Discogs personal access token. With `discogs` in the beets config's
     * plugins, the importer consults Discogs when MusicBrainz has no strong
     * match; without a token it does not.
     */
    discogsToken?: pulumi.Input<string>;
    /** Pod labels allowed to reach the internal port. */
    gatewayPodLabels?: Record<string, string>;
    /** Pod labels allowed to scrape the metrics port. */
    scraperPodLabels?: Record<string, string>;
  },
) {
  const conf = SlskConfSchema.parse(confArgs);
  if (!insideMediaRoot(conf)) {
    throw new Error(`slsk: library, downloads and shares must all be inside mediaRoot (${conf.mediaRoot})`);
  }
  const options = { provider };
  const state = conf.statePath;
  const shares = conf.shares ?? [conf.library];
  const downloads = conf.downloads.replace(/\/+$/, "");

  /**
   * The drive, as one local volume and its claim. Retain: a local volume is
   * someone's disk, and deleting the claim must never be read as permission
   * to touch what is on it.
   */
  const pv = new k8s.core.v1.PersistentVolume(
    "slsk-media",
    {
      metadata: { name: "slsk-media" },
      spec: {
        accessModes: ["ReadWriteOnce"],
        capacity: { storage: conf.capacity },
        local: { path: conf.mediaRoot },
        nodeAffinity: {
          required: {
            nodeSelectorTerms: [
              { matchExpressions: [{ key: "kubernetes.io/hostname", operator: "In", values: [opts.node] }] },
            ],
          },
        },
        persistentVolumeReclaimPolicy: "Retain",
        storageClassName: "local-storage",
        volumeMode: "Filesystem",
      },
    },
    options,
  );
  const claim = new k8s.core.v1.PersistentVolumeClaim(
    "slsk-media",
    {
      metadata: { name: "slsk-media", namespace },
      spec: {
        accessModes: ["ReadWriteOnce"],
        storageClassName: "local-storage",
        volumeName: pv.metadata.name,
        resources: { requests: { storage: conf.capacity } },
      },
    },
    options,
  );
  const mediaMounts = [{ name: "media", mountPath: conf.mediaRoot }];

  const secret = new k8s.core.v1.Secret(
    "slsk-secrets",
    {
      metadata: { name: SECRETS_NAME, namespace },
      stringData: {
        "oidc-client-secret": pulumi.output(opts.oidcClientSecret),
        "seal-key": pulumi.output(opts.sealKey),
        "database-password": pulumi.output(opts.databasePassword),
        "database-url": pulumi.interpolate`postgres://slsk:${opts.databasePassword}@127.0.0.1:5432/slsk`,
        ...(opts.account && {
          "slsk-username": pulumi.output(opts.account.username),
          "slsk-password": pulumi.output(opts.account.password),
        }),
        ...(opts.discogsToken && {
          "discogs-token": pulumi.output(opts.discogsToken),
        }),
      },
    },
    options,
  );

  const beets = conf.beetsConfig
    ? new k8s.core.v1.ConfigMap(
        "slsk-beets",
        {
          metadata: { name: "slsk-beets", namespace },
          data: { "config.yaml": conf.beetsConfig },
        },
        options,
      )
    : undefined;


  const deployment = new k8s.apps.v1.Deployment(
    NAME,
    {
      metadata: { name: NAME, namespace },
      spec: {
        replicas: 1,
        // hostPath mounts and one peer port on one node: a surge would put two
        // clients on the same account, the same directories and the same port.
        strategy: { type: "Recreate" },
        selector: { matchLabels: { app: NAME } },
        template: {
          metadata: {
            labels: { app: NAME },
            annotations: {
              "prometheus.io/scrape": "true",
              "prometheus.io/port": String(METRICS_PORT),
              "prometheus.io/path": "/metrics",
              // Restart when the importer's config changes; a mounted file
              // alone would be read once, at start.
              ...(conf.beetsConfig && { "slsk/beets-config": hash(conf.beetsConfig) }),
            },
          },
          spec: {
            // Stopping waits for an import in progress: moving an album into
            // the library is not atomic.
            terminationGracePeriodSeconds: 600,
            automountServiceAccountToken: false,
            enableServiceLinks: false,
            // Every name this resolves is public — the Soulseek server, peers,
            // MusicBrainz, the Cover Art Archive, the identity provider — and
            // the policy below forbids reaching anything in the cluster, so the
            // cluster resolver could only answer names it may not talk to.
            dnsPolicy: "None",
            dnsConfig: {
              nameservers: ["1.1.1.1", "9.9.9.9"],
              options: [
                { name: "timeout", value: "2" },
                { name: "attempts", value: "3" },
              ],
            },
            nodeSelector: { "kubernetes.io/hostname": opts.node },
            // The media tree is 1000:1000 throughout; what this writes into it
            // must be manageable by everything else that serves it. No
            // fsGroup, which would walk and chown the whole library.
            securityContext: {
              runAsUser: 1000,
              runAsGroup: 1000,
              runAsNonRoot: true,
              seccompProfile: { type: "RuntimeDefault" },
            },
            // The one thing that runs as root, with only the ownership
            // capabilities and only the state and download directories
            // mounted: kubelet creates a missing hostPath as root, and this
            // pod cannot write what root owns.
            initContainers: [
              {
                name: "state",
                image: VERSIONS.alpine,
                command: [
                  "sh",
                  "-c",
                  `test -d ${conf.library} && mkdir -p ${state}/postgres ${state}/data ${downloads}/incomplete ${downloads}/complete && chown 1000:1000 ${state} ${state}/postgres ${state}/data ${downloads} ${downloads}/incomplete ${downloads}/complete && chmod 700 ${state}/postgres`,
                ],
                securityContext: {
                  runAsUser: 0,
                  runAsNonRoot: false,
                  allowPrivilegeEscalation: false,
                  capabilities: { drop: ["ALL"], add: ["CHOWN", "FOWNER", "DAC_OVERRIDE"] },
                },
                resources: { requests: { cpu: "5m", memory: "8Mi" }, limits: { memory: "32Mi" } },
                volumeMounts: [
                  { name: "state", mountPath: state },
                  { name: "media", mountPath: conf.mediaRoot },
                ],
              },
            ],
            containers: [
              {
                name: "postgres",
                image: VERSIONS.postgres,
                imagePullPolicy: "IfNotPresent",
                // Loopback only: the pod's own containers are the only clients.
                args: ["-c", "listen_addresses=127.0.0.1"],
                env: [
                  { name: "POSTGRES_USER", value: "slsk" },
                  { name: "POSTGRES_DB", value: "slsk" },
                  { name: "POSTGRES_PASSWORD", ...secretRef("database-password") },
                  { name: "PGDATA", value: `${state}/postgres/pgdata` },
                ],
                readinessProbe: {
                  exec: { command: ["pg_isready", "-h", "127.0.0.1", "-U", "slsk"] },
                  periodSeconds: 10,
                },
                resources: {
                  limits: conf.postgres.limits,
                  ...(conf.postgres.requests && { requests: conf.postgres.requests }),
                },
                securityContext: {
                  allowPrivilegeEscalation: false,
                  capabilities: { drop: ["ALL"] },
                },
                volumeMounts: [{ name: "state", mountPath: state }],
              },
              {
                name: NAME,
                image: VERSIONS.slsk,
                imagePullPolicy: "IfNotPresent",
                ports: [
                  { name: "ui", containerPort: UI_PORT },
                  { name: "internal", containerPort: INTERNAL_PORT },
                  { name: "metrics", containerPort: METRICS_PORT },
                  {
                    name: "peer",
                    protocol: "TCP",
                    containerPort: conf.listenPort,
                    hostPort: conf.listenPort,
                  },
                ],
                env: [
                  { name: "DATABASE_URL", ...secretRef("database-url") },
                  { name: "SEAL_KEY", ...secretRef("seal-key") },
                  { name: "LIBRARY_DIR", value: conf.library },
                  { name: "SHARE_DIRS", value: shares.join(",") },
                  { name: "STATE_DIR", value: `${state}/data` },
                  { name: "STAGING_DIR", value: `${downloads}/incomplete` },
                  { name: "COMPLETE_DIR", value: `${downloads}/complete` },
                  { name: "LISTEN_PORT", value: String(conf.listenPort) },
                  { name: "UPLOAD_SLOTS", value: String(conf.uploadSlots) },
                  { name: "UPLOAD_LIMIT", value: String(conf.uploadLimit) },
                  { name: "DOWNLOAD_LIMIT", value: String(conf.downloadLimit) },
                  { name: "UI_ADDR", value: `0.0.0.0:${UI_PORT}` },
                  { name: "INTERNAL_ADDR", value: `0.0.0.0:${INTERNAL_PORT}` },
                  { name: "METRICS_ADDR", value: `0.0.0.0:${METRICS_PORT}` },
                  { name: "PUBLIC_URL", value: `https://${opts.hostname}` },
                  { name: "OIDC_ISSUER", value: opts.oidc.issuer },
                  { name: "OIDC_CLIENT_ID", value: opts.oidc.clientId },
                  { name: "OIDC_CLIENT_SECRET", ...secretRef("oidc-client-secret") },
                  { name: "DESCRIPTION", value: conf.description },
                  ...(beets ? [{ name: "BEETS_CONFIG", value: "/etc/slsk/beets/config.yaml" }] : []),
                  ...(opts.account
                    ? [
                        { name: "SLSK_USERNAME", ...secretRef("slsk-username") },
                        { name: "SLSK_PASSWORD", ...secretRef("slsk-password") },
                      ]
                    : []),
                  ...(opts.discogsToken
                    ? [{ name: "DISCOGS_TOKEN", ...secretRef("discogs-token") }]
                    : []),
                ],
                readinessProbe: {
                  httpGet: { path: "/healthz", port: UI_PORT },
                  initialDelaySeconds: 5,
                  periodSeconds: 10,
                },
                resources: {
                  limits: conf.limits,
                  ...(conf.requests && { requests: conf.requests }),
                },
                securityContext: {
                  allowPrivilegeEscalation: false,
                  readOnlyRootFilesystem: true,
                  capabilities: { drop: ["ALL"] },
                },
                volumeMounts: [
                  ...mediaMounts,
                  { name: "state", mountPath: state },
                  ...(beets ? [{ name: "beets", mountPath: "/etc/slsk/beets", readOnly: true }] : []),
                ],
              },
            ],
            volumes: [
              { name: "media", persistentVolumeClaim: { claimName: claim.metadata.name } },
              { name: "state", hostPath: { path: state, type: "DirectoryOrCreate" } },
              ...(beets ? [{ name: "beets", configMap: { name: "slsk-beets" } }] : []),
            ],
          },
        },
      },
    },
    { dependsOn: [secret, claim, ...(beets ? [beets] : [])], deleteBeforeReplace: true, provider },
  );

  if (conf.upnp) {
    const lease = 3600;
    new k8s.apps.v1.Deployment(
      "slsk-portmap",
      {
        metadata: { name: "slsk-portmap", namespace },
        spec: {
          replicas: 1,
          selector: { matchLabels: { app: "slsk-portmap" } },
          template: {
            metadata: { labels: { app: "slsk-portmap" } },
            spec: {
              automountServiceAccountToken: false,
              // SSDP discovery is multicast on the LAN, which the pod network
              // does not carry. This pod mounts nothing and holds no secret, so
              // leaving the policy's reach costs nothing.
              hostNetwork: true,
              dnsPolicy: "None",
              dnsConfig: { nameservers: ["1.1.1.1", "9.9.9.9"] },
              nodeSelector: { "kubernetes.io/hostname": opts.node },
              containers: [
                {
                  name: "portmap",
                  image: VERSIONS.alpine,
                  command: ["sh", "-c", PORTMAP],
                  env: [
                    { name: "PORT", value: String(conf.listenPort) },
                    { name: "LEASE", value: String(lease) },
                  ],
                  securityContext: { allowPrivilegeEscalation: false },
                  resources: {
                    requests: { cpu: "5m", memory: "16Mi" },
                    limits: { memory: "128Mi" },
                  },
                },
              ],
            },
          },
        },
      },
      options,
    );
  }

  new k8s.core.v1.Service(
    "slsk-service",
    {
      metadata: { name: "slsk-service", namespace },
      spec: { selector: { app: NAME }, ports: [{ port: 80, targetPort: UI_PORT }] },
    },
    options,
  );
  const internal = new k8s.core.v1.Service(
    "slsk-internal",
    {
      metadata: { name: "slsk-internal", namespace },
      spec: {
        selector: { app: NAME },
        ports: [{ port: INTERNAL_PORT, targetPort: INTERNAL_PORT }],
      },
    },
    options,
  );

  new k8s.networking.v1.NetworkPolicy(
    "slsk-netpol",
    {
      metadata: { name: NAME, namespace },
      spec: {
        podSelector: { matchLabels: { app: NAME } },
        policyTypes: ["Ingress", "Egress"],
        ingress: [
          // The UI, through Traefik; the node for the kubelet's probes.
          {
            from: [
              { podSelector: { matchLabels: { "app.kubernetes.io/name": "traefik" } } },
              { ipBlock: { cidr: "192.168.0.0/16" } },
            ],
            ports: [{ protocol: "TCP", port: UI_PORT }],
          },
          // Headers carrying credentials are believed here: the gateway only.
          {
            from: [
              { podSelector: { matchLabels: opts.gatewayPodLabels ?? { app: "mcp-gateway" } } },
            ],
            ports: [{ protocol: "TCP", port: INTERNAL_PORT }],
          },
          // The metrics agent, to /metrics and nothing else.
          {
            from: [
              { podSelector: { matchLabels: opts.scraperPodLabels ?? { app: "metrics-vmagent" } } },
            ],
            ports: [{ protocol: "TCP", port: METRICS_PORT }],
          },
          // Peers, from anywhere: that is what a peer port is.
          {
            from: [{ ipBlock: { cidr: "0.0.0.0/0" } }],
            ports: [{ protocol: "TCP", port: conf.listenPort }],
          },
        ],
        egress: [
          {
            to: [
              {
                namespaceSelector: {
                  matchLabels: { "kubernetes.io/metadata.name": "kube-system" },
                },
              },
            ],
            ports: [
              { protocol: "UDP", port: 53 },
              { protocol: "TCP", port: 53 },
            ],
          },
          // The Soulseek network, MusicBrainz and the identity provider are
          // all public; nothing inside the cluster or the house is.
          { to: [{ ipBlock: { cidr: "0.0.0.0/0", except: PRIVATE } }] },
        ],
      },
    },
    options,
  );

  return {
    routes: [{ service: NAME, hostname: opts.hostname }],
    oidc: {
      id: opts.oidc.clientId,
      name: "slsk",
      redirectUri: `https://${opts.hostname}/auth/callback`,
    },
    deployment,
    /** Where the gateway's registry points. */
    internal: {
      url: pulumi.interpolate`http://${internal.metadata.name}:${INTERNAL_PORT}`,
      mcpPath: "/mcp",
      graphqlPath: "/graphql",
    },
  } satisfies Deployed & Record<string, unknown>;
}

/** Enough of a hash to notice a change; not a security property. */
function hash(s: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h.toString(16);
}
