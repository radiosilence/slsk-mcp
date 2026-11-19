import * as k8s from "@pulumi/kubernetes";
import * as pulumi from "@pulumi/pulumi";
import * as z from "zod";
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

/**
 * Keeps a UPnP forward for the peer port on the router, renewed at half the
 * lease so a router that forgets mappings on reboot gets it back.
 *
 * `miniupnpc` comes from the stock alpine image's package mirror rather than an
 * image of our own: it is one binary invoked in a loop. The cost is the
 * hand-bumped alpine digest in `VERSIONS`, which must stay on a supported
 * branch, 3.24 or later, for miniupnpc fixes to arrive. `apk` needs root, but
 * `upnpc` parses whatever answers on the LAN, which is where miniupnpc's CVEs
 * lie, so an init container installs it as root into a shared volume and this
 * script runs as nobody with that volume read-only, loading the volume's
 * libraries for `upnpc` alone.
 */
const PORTMAP = `set -eu
lan() { ip -4 route get 1.1.1.1 | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}'; }
while :; do
  ip=$(lan)
  if [ -z "$ip" ]; then
    echo "WARNING: no route out, so no LAN address to map TCP $PORT to; retrying in 60s"
    sleep 60
    continue
  fi
  # stderr alone: stdout is a banner and the WAN address.
  if err=$(LD_LIBRARY_PATH=/deps/usr/lib /deps/usr/bin/upnpc -e slsk -a "$ip" "$PORT" "$PORT" TCP "$LEASE" 2>&1 >/dev/null); then
    echo "mapped TCP $PORT -> $ip:$PORT for $LEASE""s"
  else
    # Its last line, as printable ASCII and capped, because devices on the LAN
    # choose what upnpc prints. A range rather than [:print:], which busybox tr
    # reads as literal characters.
    echo "WARNING: could not map TCP $PORT; peers behind their own NAT cannot reach us: $(printf '%s' "$err" | tail -n 1 | tr -cd ' -~' | head -c 300)"
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
 * slsk-mcp: a Soulseek client and a UPnP mapper for its peer port. Its state
 * (jobs, sealed credentials, history) is a SQLite file in `statePath`, on the
 * node's own disk.
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
    /**
     * @deprecated Ignored: state is in SQLite since 0.1.56, and nothing reads
     * a password. Kept so a caller still passing it compiles; remove it from
     * the call.
     */
    databasePassword?: pulumi.Input<string>;
    /**
     * An account to log in with at start, so the client is sharing before
     * anyone asks it anything. Credentials from the gateway or the UI replace
     * it.
     */
    account?: { username: pulumi.Input<string>; password: pulumi.Input<string> };
    /**
     * A Discogs personal access token, generated under Settings → Developers
     * on discogs.com. With it the importer consults Discogs when MusicBrainz
     * has no strong match, and Discogs releases can be chosen in review; no
     * beets config change is needed. Without it only MusicBrainz is used.
     */
    discogsToken?: pulumi.Input<string>;
    /**
     * An AcoustID application key, registered at
     * acoustid.org/new-application. With it the importer identifies folders
     * no release matches strongly by fingerprinting their audio.
     */
    acoustidClient?: pulumi.Input<string>;
    /** Pod labels of the ingress controller allowed to reach the UI. Traefik's by default. */
    ingressPodLabels?: Record<string, string>;
    /**
     * Labels of the namespace the ingress controller runs in. Without them
     * the pod labels match only in this namespace; `{}` matches any.
     */
    ingressNamespaceLabels?: Record<string, string>;
    /** Where the nodes' addresses lie, so the kubelet's probes are admitted. */
    nodeCidr?: string;
    /** Pod labels allowed to reach the internal port. */
    gatewayPodLabels?: Record<string, string>;
    /** The gateway's namespace labels, as `ingressNamespaceLabels`. */
    gatewayNamespaceLabels?: Record<string, string>;
    /** Pod labels allowed to scrape the metrics port. */
    scraperPodLabels?: Record<string, string>;
    /** The scraper's namespace labels, as `ingressNamespaceLabels`. */
    scraperNamespaceLabels?: Record<string, string>;
  },
) {
  const conf = SlskConfSchema.parse(confArgs);
  const nodeCidr = z.cidrv4().parse(opts.nodeCidr ?? "192.168.0.0/16");
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
        ...(opts.account && {
          "slsk-username": pulumi.output(opts.account.username),
          "slsk-password": pulumi.output(opts.account.password),
        }),
        ...(opts.discogsToken && {
          "discogs-token": pulumi.output(opts.discogsToken),
        }),
        ...(opts.acoustidClient && {
          "acoustid-client": pulumi.output(opts.acoustidClient),
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
                  'test -d "$LIBRARY" && mkdir -p "$STATE/data" "$DOWNLOADS/incomplete" "$DOWNLOADS/complete" && chown 1000:1000 "$STATE" "$STATE/data" "$DOWNLOADS" "$DOWNLOADS/incomplete" "$DOWNLOADS/complete" && chmod 700 "$STATE/data"',
                ],
                env: [
                  { name: "LIBRARY", value: conf.library },
                  { name: "STATE", value: state },
                  { name: "DOWNLOADS", value: downloads },
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
                  ...(opts.acoustidClient
                    ? [{ name: "ACOUSTID_CLIENT", ...secretRef("acoustid-client") }]
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
              // does not carry. No NetworkPolicy selects this pod, and on the
              // host network it shares the node's network namespace, so it can
              // reach whatever the node can: the LAN, the internet, the
              // tailnet, the API server and listeners on the node's loopback.
              // What bounds it is what it holds: only its own scratch volume,
              // no secret or service-account token, and a loop running as
              // nobody with no capabilities on a read-only root.
              hostNetwork: true,
              dnsPolicy: "None",
              dnsConfig: { nameservers: ["1.1.1.1", "9.9.9.9"] },
              nodeSelector: { "kubernetes.io/hostname": opts.node },
              securityContext: { seccompProfile: { type: "RuntimeDefault" } },
              volumes: [{ name: "deps", emptyDir: {} }],
              initContainers: [
                {
                  name: "deps",
                  image: VERSIONS.alpine,
                  // Into `/deps` from the image's own keys and repositories,
                  // leaving the image untouched. Package scripts would have to
                  // chroot into `/deps`, which has no shell and this container
                  // no CAP_SYS_CHROOT, so they are skipped.
                  command: [
                    "apk",
                    "add",
                    "--no-cache",
                    "--no-scripts",
                    "--root",
                    "/deps",
                    "--initdb",
                    "--keys-dir",
                    "/etc/apk/keys",
                    "--repositories-file",
                    "/etc/apk/repositories",
                    "miniupnpc",
                  ],
                  // Root, because apk will not initialise its database or set
                  // file ownership as anyone else. It needs no capability for
                  // either, so none is kept, and it writes nothing outside
                  // `/deps`, so the image stays read-only.
                  securityContext: {
                    runAsUser: 0,
                    runAsNonRoot: false,
                    allowPrivilegeEscalation: false,
                    readOnlyRootFilesystem: true,
                    capabilities: { drop: ["ALL"] },
                  },
                  volumeMounts: [{ name: "deps", mountPath: "/deps" }],
                  // Unpacking a package peaks far above what the loop needs.
                  resources: {
                    requests: { cpu: "5m", memory: "16Mi" },
                    limits: { memory: "128Mi" },
                  },
                },
              ],
              containers: [
                {
                  name: "portmap",
                  // The same image as the init container: the libraries in
                  // `/deps` are loaded by this image's own musl.
                  image: VERSIONS.alpine,
                  command: ["sh", "-c", PORTMAP],
                  env: [
                    { name: "PORT", value: String(conf.listenPort) },
                    { name: "LEASE", value: String(lease) },
                  ],
                  securityContext: {
                    runAsUser: 65534,
                    runAsGroup: 65534,
                    runAsNonRoot: true,
                    allowPrivilegeEscalation: false,
                    readOnlyRootFilesystem: true,
                    capabilities: { drop: ["ALL"] },
                  },
                  volumeMounts: [{ name: "deps", mountPath: "/deps", readOnly: true }],
                  // A sleeping shell that wakes every half-lease to run
                  // `upnpc` once.
                  resources: {
                    requests: { cpu: "5m", memory: "8Mi" },
                    limits: { memory: "32Mi" },
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
          // The UI, through the ingress controller; the node for the
          // kubelet's probes.
          {
            from: [
              peer(opts.ingressPodLabels ?? { "app.kubernetes.io/name": "traefik" }, opts.ingressNamespaceLabels),
              { ipBlock: { cidr: nodeCidr } },
            ],
            ports: [{ protocol: "TCP", port: UI_PORT }],
          },
          // Headers carrying credentials are believed here: the gateway only.
          {
            from: [peer(opts.gatewayPodLabels ?? { app: "mcp-gateway" }, opts.gatewayNamespaceLabels)],
            ports: [{ protocol: "TCP", port: INTERNAL_PORT }],
          },
          // The metrics agent, to /metrics and nothing else.
          {
            from: [peer(opts.scraperPodLabels ?? { app: "metrics-vmagent" }, opts.scraperNamespaceLabels)],
            ports: [{ protocol: "TCP", port: METRICS_PORT }],
          },
          // Peers, from anywhere: that is what a peer port is.
          {
            from: [{ ipBlock: { cidr: "0.0.0.0/0" } }],
            ports: [{ protocol: "TCP", port: conf.listenPort }],
          },
        ],
        egress: [
          // The Soulseek network, MusicBrainz, the identity provider and the
          // public resolvers in `dnsConfig` are all public; nothing inside the
          // cluster or the house is.
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

/** Pods by label, in this namespace unless `namespaces` names others. */
function peer(pods: Record<string, string>, namespaces?: Record<string, string>) {
  return {
    podSelector: { matchLabels: pods },
    ...(namespaces && { namespaceSelector: { matchLabels: namespaces } }),
  };
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
