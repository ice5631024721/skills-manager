import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  AlertTriangle,
  ArrowDownToLine,
  ArrowUpCircle,
  Check,
  Globe,
  Loader2,
  Pencil,
  Plug,
  Plus,
  RefreshCw,
  ScrollText,
  Terminal,
  Trash2,
  X,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { AgentIcon } from "../components/AgentIcon";
import { ConfirmDialog } from "../components/ConfirmDialog";
import { McpAddDialog } from "../components/McpAddDialog";
import { McpDriftDialog } from "../components/McpDriftDialog";
import type { McpDriftChain } from "../components/McpDriftDialog";
import { McpAgentDots } from "../components/McpAgentDots";
import { cn } from "../utils";
import { getErrorMessage } from "../lib/error";
import * as api from "../lib/tauri";
import type {
  McpAgentStatus,
  McpInventoryReport,
  McpServerDto,
  McpServerSummary,
  UpgradePlan,
} from "../lib/tauri";

/** Transport badge palette — local processes vs network endpoints. */
function transportClassName(transport: string): string {
  if (transport === "stdio") {
    return "bg-sky-500/10 text-sky-700 dark:text-sky-300";
  }
  if (transport === "http" || transport === "streamable-http") {
    return "bg-violet-500/10 text-violet-700 dark:text-violet-300";
  }
  return "bg-surface-hover text-muted";
}

/** Last path segment — cards stay narrow, the full path lives in the tooltip. */
function fileName(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] || path;
}

// ── Managed library (ADR-0006) ──

/** Open/prefill intent for the add/edit dialog (Task 11). */
type McpAddDialogState = { mode: "add" } | { mode: "edit"; server: McpServerDto };

/** Confirmation the user requested; the upgrade plan is fetched up front so
 *  the dialog can show the exact commands verbatim (ADR-0006 §1). */
type McpConfirmRequest =
  | { kind: "upgrade"; server: McpServerDto; plan: UpgradePlan }
  | { kind: "delete"; server: McpServerDto };

/** One-shot liveness verdict as a header glyph (CONTEXT.md: **Probe**). */
function ProbeIndicator({ server }: { server: McpServerDto }) {
  const { t } = useTranslation();
  if (server.probe_status === "ok") {
    return (
      <span
        className="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-emerald-500"
        title={server.probe_message ?? t("mcp.probePassed")}
      >
        <Check className="h-3.5 w-3.5" strokeWidth={3} />
      </span>
    );
  }
  if (server.probe_status === "fail") {
    return (
      <span
        className="inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center text-red-500"
        title={server.probe_message ?? t("mcp.probeFailed")}
      >
        <X className="h-3.5 w-3.5" strokeWidth={3} />
      </span>
    );
  }
  return (
    <span
      className="h-2 w-2 shrink-0 rounded-full bg-surface-active"
      title={t("mcp.probePending")}
    />
  );
}

interface McpLibrarySectionProps {
  servers: McpServerDto[];
  /** From the inventory scan: the dots mirror "Detected servers" agents. */
  agents: McpAgentStatus[];
  openAddDialog: () => void;
  editServer: (server: McpServerDto) => void;
  /** Fetches the upgrade plan, then opens the verbatim-command confirm. */
  upgradeServer: (server: McpServerDto) => Promise<void>;
  deleteServer: (server: McpServerDto) => void;
  /** A sync/unsync write met drifted files: the drift dialog replays it. */
  requestDriftChain: (chain: McpDriftChain) => void;
  /** Re-pull the library after a write/probe changed backend state. */
  onChanged: () => void;
}

/** Card grid for the definition library: each managed server with its
 *  per-agent sync dots, probe verdict, update badge and actions. */
function ManagedLibrarySection({
  servers,
  agents,
  openAddDialog,
  editServer,
  upgradeServer,
  deleteServer,
  requestDriftChain,
  onChanged,
}: McpLibrarySectionProps) {
  const { t } = useTranslation();
  // Which (server, agent) sync write is in flight; only that dot spins.
  const [busyToggle, setBusyToggle] = useState<{ serverId: string; agentKey: string } | null>(
    null,
  );
  const [busyProbe, setBusyProbe] = useState<string | null>(null);
  const [busyUpgrade, setBusyUpgrade] = useState<string | null>(null);

  const agentName = useCallback(
    (agentKey: string) =>
      agents.find((agent) => agent.agent_key === agentKey)?.display_name ?? agentKey,
    [agents],
  );

  const syncToast = useCallback(
    (server: McpServerDto, agentKey: string, nextDesired: boolean) =>
      toast.success(
        t(nextDesired ? "mcp.syncedToAgent" : "mcp.unsyncedFromAgent", {
          name: server.name,
          agent: agentName(agentKey),
        }),
      ),
    [agentName, t],
  );

  const handleToggle = useCallback(
    async (server: McpServerDto, agentKey: string, nextDesired: boolean) => {
      setBusyToggle({ serverId: server.id, agentKey });
      // One lane per (server, agent): the same call, token-optional, drives
      // both the first attempt and every drift-approved replay.
      const write = (approvedDrift?: string | null) =>
        nextDesired
          ? api.syncMcpToAgent(server.id, agentKey, approvedDrift)
          : api.unsyncMcpFromAgent(server.id, agentKey, approvedDrift);
      try {
        const outcome = await write();
        if (outcome.status === "pending_drift") {
          // The drifted file stays untouched until the current-vs-planned
          // confirmation; cancelling replays nothing and just re-pulls.
          requestDriftChain({
            name: server.name,
            queue: [outcome],
            retry: write,
            agentLabel: agentName,
            onDone: ({ cancelled }) => {
              if (cancelled === null) syncToast(server, agentKey, nextDesired);
              else toast.info(t("mcp.driftStopped", { name: server.name }));
              onChanged();
            },
          });
          return;
        }
        syncToast(server, agentKey, nextDesired);
        onChanged();
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
        onChanged();
      } finally {
        setBusyToggle(null);
      }
    },
    [agentName, onChanged, requestDriftChain, syncToast, t],
  );

  const handleProbe = useCallback(
    async (server: McpServerDto) => {
      setBusyProbe(server.id);
      try {
        const state = await api.probeMcpServer(server.id);
        const message = state.probe_message ?? "";
        if (state.probe_status === "ok") {
          toast.success(t("mcp.probePassedToast", { name: server.name, message }));
        } else {
          toast.error(t("mcp.probeFailedToast", { name: server.name, message }));
        }
        onChanged();
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
      } finally {
        setBusyProbe(null);
      }
    },
    [onChanged, t],
  );

  const actionClass =
    "rounded-md p-1.5 text-muted transition-colors hover:bg-surface-hover hover:text-primary disabled:opacity-50";

  const renderCard = (server: McpServerDto) => {
    const endpoint = server.command
      ? [server.command, ...server.args].join(" ")
      : server.url ?? "";
    // Key names only, never values (ADR-0006: env values stay masked).
    const envKeys = Object.keys(server.env);
    const hasUpdate = server.update_status === "update_available";
    const isProbing = busyProbe === server.id;

    return (
      <div
        key={server.id}
        className="app-panel group relative flex h-full flex-col shadow-card transition-all hover:-translate-y-px hover:border-border hover:shadow-card-hover"
      >
        <div className="flex items-center gap-2 px-3.5 pt-3 pb-1.5">
          <ProbeIndicator server={server} />
          <h3
            className="flex-1 truncate text-[14px] font-semibold text-primary group-hover:text-accent-light"
            title={server.name}
          >
            {server.name}
          </h3>
          {hasUpdate && (
            <span
              className="shrink-0 rounded-full bg-amber-500/10 px-2 py-0.5 text-[11px] font-medium text-amber-700 dark:text-amber-300"
              title={
                server.remote_version
                  ? t("mcp.remoteVersion", { version: server.remote_version })
                  : undefined
              }
            >
              {t("mcp.updateAvailable")}
            </span>
          )}
          <span
            className={cn(
              "inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[11px] font-medium",
              transportClassName(server.transport)
            )}
          >
            {server.transport}
          </span>
        </div>

        <div className="px-3.5 pb-3">
          {endpoint ? (
            <div className="flex items-center gap-1.5 text-[12px] text-muted" title={endpoint}>
              {server.command ? (
                <Terminal className="h-3.5 w-3.5 shrink-0 text-faint" />
              ) : (
                <Globe className="h-3.5 w-3.5 shrink-0 text-faint" />
              )}
              <span className="truncate font-mono">{endpoint}</span>
            </div>
          ) : (
            <div className="text-[12px] text-faint">{t("mcp.noEndpoint")}</div>
          )}

          {envKeys.length > 0 && (
            <div className="mt-2 flex flex-wrap items-center gap-1">
              {envKeys.map((key) => (
                <span
                  key={key}
                  className="inline-flex items-center rounded-full border border-border-subtle bg-surface-hover px-2 py-0.5 font-mono text-[11px] text-muted"
                >
                  {key}
                </span>
              ))}
              <span className="text-[11px] text-faint">{t("mcp.envKeysOnly")}</span>
            </div>
          )}
        </div>

        <div className="mt-auto flex items-center justify-between gap-2 border-t border-border-faint px-3.5 py-2.5">
          <McpAgentDots
            agents={agents}
            bindings={server.bindings}
            size="sm"
            onToggle={(agentKey, nextDesired) => void handleToggle(server, agentKey, nextDesired)}
            pendingKey={
              busyToggle && busyToggle.serverId === server.id ? busyToggle.agentKey : null
            }
          />
          <div className="flex shrink-0 items-center gap-1">
            <button
              type="button"
              title={t("mcp.editServer")}
              aria-label={t("mcp.editServer")}
              onClick={() => editServer(server)}
              className={actionClass}
            >
              <Pencil className="h-3.5 w-3.5" />
            </button>
            <button
              type="button"
              title={t("mcp.probeNow")}
              aria-label={t("mcp.probeNow")}
              disabled={isProbing}
              onClick={() => void handleProbe(server)}
              className={actionClass}
            >
              {isProbing ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <Activity className="h-3.5 w-3.5" />
              )}
            </button>
            {hasUpdate && (
              <button
                type="button"
                title={t("mcp.upgrade")}
                aria-label={t("mcp.upgrade")}
                disabled={busyUpgrade === server.id}
                onClick={() => {
                  setBusyUpgrade(server.id);
                  void upgradeServer(server).finally(() => setBusyUpgrade(null));
                }}
                className={cn(actionClass, "text-amber-600 dark:text-amber-400")}
              >
                {busyUpgrade === server.id ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                ) : (
                  <ArrowUpCircle className="h-3.5 w-3.5" />
                )}
              </button>
            )}
            <button
              type="button"
              title={t("mcp.deleteServer")}
              aria-label={t("mcp.deleteServer")}
              onClick={() => deleteServer(server)}
              className={cn(actionClass, "hover:text-red-500")}
            >
              <Trash2 className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      </div>
    );
  };

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-3">
        <div className="app-section-title">{t("mcp.libraryTitle")}</div>
        <button
          type="button"
          onClick={openAddDialog}
          className="app-toolbar-button app-toolbar-button-secondary"
        >
          <Plus className="h-3.5 w-3.5" />
          {t("mcp.addServer")}
        </button>
      </div>
      {servers.length === 0 ? (
        <div className="app-panel border-dashed px-4 py-8 text-center text-[13px] text-muted">
          {t("mcp.libraryEmpty")}
        </div>
      ) : (
        <div className="grid grid-cols-2 gap-3 lg:grid-cols-3">{servers.map(renderCard)}</div>
      )}
    </div>
  );
}

export function McpInventory() {
  const { t } = useTranslation();
  const [report, setReport] = useState<McpInventoryReport | null>(null);
  const [libraryServers, setLibraryServers] = useState<McpServerDto[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // One lane busy at a time: add/edit, an upgrade/delete confirm, or a
  // running drift-confirmation chain.
  const [addDialog, setAddDialog] = useState<McpAddDialogState | null>(null);
  const [confirmRequest, setConfirmRequest] = useState<McpConfirmRequest | null>(null);
  const [driftChain, setDriftChain] = useState<McpDriftChain | null>(null);

  const scan = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [inventory, library] = await Promise.all([
        api.getMcpInventory(),
        api.getMcpLibrary(),
      ]);
      setReport(inventory);
      setLibraryServers(library.servers);
    } catch (err) {
      setError(getErrorMessage(err, t("mcp.agent.readFailed")));
    } finally {
      setLoading(false);
    }
  }, [t]);

  /** Event-driven re-pull after a write/probe; errors surface as toasts so
   *  the triggering action's outcome is never hidden behind a reload. */
  const refreshLibrary = useCallback(async () => {
    try {
      setLibraryServers((await api.getMcpLibrary()).servers);
    } catch (err) {
      toast.error(getErrorMessage(err, t("mcp.libraryLoadFailed")));
    }
  }, [t]);

  useEffect(() => {
    void scan();
  }, [scan]);

  // Startup upstream round, mirroring the skill auto-update one: fires once
  // per mount after the first successful load, non-blocking, then re-pulls
  // the library. The ref survives `loading`/`error` edges (that edge cycling
  // is what recently caused a toast loop); there is deliberately no interval
  // timer — periodic rounds are the Rust scheduler's job.
  const updateRoundRef = useRef(false);
  useEffect(() => {
    if (loading || error || updateRoundRef.current) return;
    updateRoundRef.current = true;
    void (async () => {
      try {
        await api.checkMcpUpdates(false);
        await refreshLibrary();
      } catch {
        // Silent: badges stay stale and the next rescan retries.
      }
    })();
  }, [loading, error, refreshLibrary]);

  const dialogsBusy = addDialog !== null || confirmRequest !== null || driftChain !== null;

  const agentName = useCallback(
    (agentKey: string) =>
      report?.agents.find((agent) => agent.agent_key === agentKey)?.display_name ?? agentKey,
    [report],
  );

  /** Mount a drift chain; the wrapper guarantees unmount on settle before
   *  the caller's own completion side effects run. */
  const requestDriftChain = useCallback((chain: McpDriftChain) => {
    setDriftChain({
      ...chain,
      onDone: (result) => {
        setDriftChain(null);
        chain.onDone(result);
      },
    });
  }, []);

  const openAddDialog = useCallback(() => {
    if (dialogsBusy) return;
    setAddDialog({ mode: "add" });
  }, [dialogsBusy]);

  const editServer = useCallback(
    (server: McpServerDto) => {
      if (dialogsBusy) return;
      setAddDialog({ mode: "edit", server });
    },
    [dialogsBusy],
  );

  /** Fetch the exact commands first (ADR-0006 §1: nothing runs before the
   *  user has seen them verbatim), then open the confirm. */
  const upgradeServer = useCallback(
    async (server: McpServerDto) => {
      if (dialogsBusy) return;
      try {
        const plan = await api.getMcpUpgradePlan(server.id);
        setConfirmRequest({ kind: "upgrade", server, plan });
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
      }
    },
    [dialogsBusy, t],
  );

  /** Confirm against every binding's agent file; drift-gated. */
  const deleteServer = useCallback(
    (server: McpServerDto) => {
      if (dialogsBusy) return;
      setConfirmRequest({ kind: "delete", server });
    },
    [dialogsBusy],
  );

  const runUpgrade = useCallback(
    async (server: McpServerDto) => {
      try {
        await api.applyMcpUpgrade(server.id);
        toast.success(t("mcp.upgradeDone", { name: server.name }), {
          description: t("mcp.upgradeReconnectHint"),
        });
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
      } finally {
        void refreshLibrary();
      }
    },
    [refreshLibrary, t],
  );

  const runDelete = useCallback(
    async (server: McpServerDto) => {
      try {
        const outcome = await api.deleteMcpServer(server.id);
        if (outcome.pending_drift.length > 0) {
          // Some agents' files drifted: they wait on the chain, and the
          // definition row survives until the last one is cleared.
          requestDriftChain({
            name: server.name,
            queue: outcome.pending_drift,
            applied: outcome.applied,
            retry: (token) => api.deleteMcpServer(server.id, token),
            agentLabel: agentName,
            onDone: ({ cancelled }) => {
              if (cancelled === null) toast.success(t("mcp.deleteDone", { name: server.name }));
              else toast.info(t("mcp.deleteKept", { name: server.name }));
              void refreshLibrary();
            },
          });
          return;
        }
        toast.success(t("mcp.deleteDone", { name: server.name }));
        void refreshLibrary();
      } catch (err) {
        // Mirrors the skill removal loop: report, then re-pull so the UI
        // shows whatever the refused call really left behind.
        toast.error(getErrorMessage(err, t("common.error")));
        void refreshLibrary();
      }
    },
    [agentName, refreshLibrary, requestDriftChain, t],
  );

  // ── take over foreign entries (ADR-0006 §7) ──

  const [busyTakeover, setBusyTakeover] = useState<string | null>(null);

  /** "agent_key::name" set of every (agent, name) a binding already covers. */
  const managedKeys = useMemo(() => {
    const keys = new Set<string>();
    for (const server of libraryServers) {
      for (const binding of server.bindings) keys.add(`${binding.agent_key}::${server.name}`);
    }
    return keys;
  }, [libraryServers]);

  const takeoverOccurrence = useCallback(
    async (agentKey: string, agentDisplayName: string, name: string) => {
      const key = `${agentKey}::${name}`;
      setBusyTakeover(key);
      try {
        await api.takeoverMcpEntry(agentKey, name);
        toast.success(t("mcp.takeoverDone", { name, agent: agentDisplayName }));
        await refreshLibrary();
      } catch (err) {
        toast.error(t("mcp.takeoverFailed", { name }), {
          description: getErrorMessage(err, t("common.error")),
        });
      } finally {
        setBusyTakeover(null);
      }
    },
    [refreshLibrary, t],
  );

  /** Mirrors a Skill Source group header: icon chip, name, count, path. */
  const renderAgentRow = (agent: McpAgentStatus) => {
    const failed = !!agent.error;
    let statusLine: string;
    if (failed) {
      statusLine = t("mcp.agent.readFailed");
    } else if (!agent.installed) {
      statusLine = t("mcp.agent.notInstalled");
    } else if (!agent.config_exists) {
      statusLine = t("mcp.agent.noConfig");
    } else {
      statusLine = t("mcp.agent.serverCount", { servers: agent.server_count });
    }

    return (
      <div
        key={agent.agent_key}
        className={cn(
          "rounded-xl border bg-background",
          failed ? "border-amber-500/40" : "border-border"
        )}
      >
        <div className="flex items-center gap-2.5 px-3 py-2.5">
          <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg border border-border-subtle bg-surface-hover text-secondary">
            <AgentIcon
              agentKey={agent.agent_key}
              displayName={agent.display_name}
              className="h-4 w-4"
            />
          </span>
          <div className="flex min-w-0 flex-1 items-center gap-2.5">
            <span className="truncate text-[14px] font-semibold text-primary">
              {agent.display_name}
            </span>
            <span className="shrink-0 rounded-full border border-border bg-surface-hover px-2 text-[11px] font-semibold leading-5 text-tertiary tabular-nums">
              {agent.server_count}
            </span>
            <span
              className="hidden min-w-0 truncate text-[12px] text-muted lg:block"
              title={agent.config_path}
            >
              {agent.config_path}
            </span>
          </div>
          <span
            className={cn(
              "shrink-0 text-[12px]",
              failed ? "text-amber-700 dark:text-amber-300" : "text-muted"
            )}
          >
            {statusLine}
          </span>
        </div>
        {agent.error && (
          <div className="flex items-start gap-1.5 border-t border-border-faint px-3 py-2 text-[11px] leading-4 text-amber-700 dark:text-amber-300">
            <AlertTriangle className="mt-px h-3 w-3 shrink-0" />
            <span className="break-all">{agent.error}</span>
          </div>
        )}
      </div>
    );
  };

  const renderServerCard = (server: McpServerSummary) => {
    const endpoint = server.command || server.url;
    // The visible wording is uniform ("registered") because only some formats
    // carry an explicit on/off field; the switch state stays in the tooltip.
    const nativeState = (occurrence: McpServerSummary["agents"][number]) =>
      occurrence.enabled === null
        ? t("mcp.state.registered")
        : occurrence.enabled
          ? t("mcp.state.enabled")
          : t("mcp.state.disabled");
    const states = server.agents.map(nativeState);
    const allDisabled =
      server.agents.length > 0 && server.agents.every((occurrence) => occurrence.enabled === false);
    const configPaths = Array.from(new Set(server.agents.map((occurrence) => occurrence.config_path)));
    // Per-agent endpoints only earn a line when they actually differ; a merged
    // server with identical commands would just repeat the line above.
    const distinctEndpoints = new Set(
      server.agents.map((occurrence) => occurrence.command || occurrence.url || "")
    );

    return (
      <div
        key={server.name}
        className="app-panel group relative flex h-full flex-col shadow-card transition-all hover:-translate-y-px hover:border-border hover:shadow-card-hover"
      >
        <div className="flex items-center gap-2.5 px-3.5 pt-3 pb-1.5">
          <span
            className={cn(
              "h-2 w-2 shrink-0 rounded-full transition-opacity",
              allDisabled ? "bg-surface-active" : "bg-accent-light shadow-[0_0_0_3px_var(--color-accent-bg)]"
            )}
            title={states.join(" · ")}
          />
          <h3
            className="flex-1 truncate text-[14px] font-semibold text-primary group-hover:text-accent-light"
            title={server.name}
          >
            {server.name}
          </h3>
          <span
            className={cn(
              "inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[11px] font-medium",
              transportClassName(server.transport)
            )}
          >
            {server.transport}
          </span>
        </div>

        <div className="px-3.5 pb-3">
          {endpoint ? (
            <div className="flex items-center gap-1.5 text-[12px] text-muted" title={endpoint}>
              {server.command ? (
                <Terminal className="h-3.5 w-3.5 shrink-0 text-faint" />
              ) : (
                <Globe className="h-3.5 w-3.5 shrink-0 text-faint" />
              )}
              <span className="truncate font-mono">{endpoint}</span>
            </div>
          ) : (
            <div className="text-[12px] text-faint">{t("mcp.noEndpoint")}</div>
          )}

          {server.env_keys.length > 0 && (
            <div className="mt-2 flex flex-wrap items-center gap-1">
              {server.env_keys.map((key) => (
                <span
                  key={key}
                  className="inline-flex items-center rounded-full border border-border-subtle bg-surface-hover px-2 py-0.5 font-mono text-[11px] text-muted"
                >
                  {key}
                </span>
              ))}
              <span className="text-[11px] text-faint">{t("mcp.envKeysOnly")}</span>
            </div>
          )}

          {distinctEndpoints.size > 1 && (
            <div className="mt-2 space-y-1 border-t border-border-faint pt-2">
              {server.agents.map((occurrence) => (
                <div
                  key={`detail-${occurrence.agent_key}`}
                  className="flex items-start gap-1.5 text-[11px] leading-4 text-faint"
                >
                  <AgentIcon
                    agentKey={occurrence.agent_key}
                    displayName={occurrence.agent_display_name}
                    className="mt-px h-3 w-3 shrink-0"
                  />
                  <span className="break-all font-mono">
                    {occurrence.command || occurrence.url || t("mcp.noEndpoint")}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>

        <div className="mt-auto flex items-center justify-between gap-2 border-t border-border-faint px-3.5 py-2.5">
          <span
            className="inline-flex min-w-0 items-center gap-1 text-[12px] text-muted"
            title={configPaths.join("\n")}
          >
            <ScrollText className="h-3 w-3 shrink-0" />
            <span className="truncate">
              {configPaths.length === 1
                ? fileName(configPaths[0])
                : t("mcp.configFiles", { count: configPaths.length })}
            </span>
          </span>
          <div className="flex shrink-0 items-center gap-1.5">
            {server.agents.map((occurrence, index) => {
              // Covered = some managed definition of this name is bound to
              // this agent (a binding is an (agent, name) fact).
              const covered = managedKeys.has(`${occurrence.agent_key}::${server.name}`);
              const takeoverKey = `${occurrence.agent_key}::${server.name}`;
              const takingOver = busyTakeover === takeoverKey;
              return (
                <Fragment key={occurrence.agent_key}>
                  {index > 0 && <span className="text-faint">·</span>}
                  {covered ? (
                    <span
                      className="inline-flex items-center gap-1 rounded-full border border-border-subtle bg-surface-hover px-1.5 text-[11px] text-tertiary"
                      title={`${occurrence.agent_display_name} · ${t("mcp.managedChip")} · ${occurrence.config_path}`}
                    >
                      <AgentIcon
                        agentKey={occurrence.agent_key}
                        displayName={occurrence.agent_display_name}
                        className="h-3.5 w-3.5"
                      />
                      {t("mcp.managedChip")}
                    </span>
                  ) : (
                    <span
                      className="inline-flex items-center gap-1 text-[12px] text-muted"
                      title={`${occurrence.agent_display_name} · ${states[index]} · ${occurrence.config_path}`}
                    >
                      <AgentIcon
                        agentKey={occurrence.agent_key}
                        displayName={occurrence.agent_display_name}
                        className="h-3.5 w-3.5"
                      />
                      {t("mcp.state.registered")}
                      {/* Quiet ghost affordance: adopting mirrors the entry
                          into the library without touching the file
                          (ADR-0006 §7). The card itself stays as before. */}
                      <button
                        type="button"
                        disabled={takingOver}
                        aria-label={t("mcp.takeover")}
                        title={t("mcp.takeover")}
                        onClick={() =>
                          void takeoverOccurrence(
                            occurrence.agent_key,
                            occurrence.agent_display_name,
                            server.name,
                          )
                        }
                        className={cn(
                          "rounded-md p-0.5 text-faint outline-none transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50",
                        )}
                      >
                        {takingOver ? (
                          <Loader2 className="h-3 w-3 animate-spin" />
                        ) : (
                          <ArrowDownToLine className="h-3 w-3" />
                        )}
                      </button>
                    </span>
                  )}
                </Fragment>
              );
            })}
          </div>
        </div>
      </div>
    );
  };

  const serverCount = report?.servers.length ?? 0;
  const agentCount = report?.agents.filter((agent) => agent.server_count > 0).length ?? 0;

  return (
    <div className="app-page">
      <div className="app-page-header pr-2 pb-1 flex items-center justify-between gap-3">
        <h1 className="app-page-title flex items-center gap-2">
          {t("mcp.title")}
          <span className="app-badge">{serverCount}</span>
        </h1>
      </div>

      <div className="app-toolbar">
        <div className="flex min-w-0 flex-col gap-1">
          <span className="text-[13px] leading-5 text-muted">{t("mcp.subtitle")}</span>
          {report && (
            <span className="text-[12px] text-faint">
              {t("mcp.summary", { servers: serverCount, agents: agentCount })}
            </span>
          )}
        </div>
        <button
          type="button"
          onClick={() => void scan()}
          disabled={loading}
          className="app-toolbar-button app-toolbar-button-secondary disabled:opacity-50"
        >
          {loading ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <RefreshCw className="h-3.5 w-3.5" />
          )}
          {loading ? t("mcp.scanning") : t("mcp.rescan")}
        </button>
      </div>

      {error && (
        <div className="app-panel flex items-start gap-2 border border-amber-500/40 bg-amber-500/5 p-3.5 text-[13px] text-amber-700 dark:text-amber-300">
          <AlertTriangle className="mt-px h-4 w-4 shrink-0" />
          <span className="break-all">{t("mcp.loadFailed", { message: error })}</span>
        </div>
      )}

      {report && (
        <div className="flex flex-col gap-4 pb-8">
          <div className="flex flex-col gap-3">
            <div className="app-section-title">{t("mcp.agentsSection")}</div>
            <div className="grid gap-3 sm:grid-cols-2">{report.agents.map(renderAgentRow)}</div>
          </div>

          <ManagedLibrarySection
            servers={libraryServers}
            agents={report.agents}
            openAddDialog={openAddDialog}
            editServer={editServer}
            upgradeServer={upgradeServer}
            deleteServer={deleteServer}
            requestDriftChain={requestDriftChain}
            onChanged={() => void refreshLibrary()}
          />

          <div className="flex flex-col gap-3">
            <div className="app-section-title">{t("mcp.serversSection")}</div>
            {serverCount === 0 ? (
              <div className="flex flex-1 flex-col items-center justify-center pb-20 text-center">
                <Plug className="mb-4 h-12 w-12 text-faint" />
                <h3 className="mb-1.5 text-[14px] font-semibold text-tertiary">
                  {t("mcp.empty.title")}
                </h3>
                <p className="max-w-md text-[13px] leading-5 text-muted">{t("mcp.empty.body")}</p>
              </div>
            ) : (
              <div className="grid grid-cols-2 gap-3 lg:grid-cols-3">
                {report.servers.map(renderServerCard)}
              </div>
            )}
          </div>
        </div>
      )}

      {!report && loading && !error && (
        <div className="flex flex-1 items-center justify-center pb-20">
          <Loader2 className="h-5 w-5 animate-spin text-muted" />
        </div>
      )}

      {addDialog && (
        <McpAddDialog
          open
          server={addDialog.mode === "edit" ? addDialog.server : null}
          onClose={() => setAddDialog(null)}
          onChanged={() => void refreshLibrary()}
          runDriftChain={requestDriftChain}
          agentLabel={agentName}
        />
      )}

      {driftChain && <McpDriftDialog chain={driftChain} />}

      {confirmRequest?.kind === "upgrade" && (
        <ConfirmDialog
          open
          tone="warning"
          title={t("mcp.upgradeConfirmTitle")}
          message={t("mcp.upgradeConfirmBody", { name: confirmRequest.server.name })}
          confirmLabel={t("mcp.upgrade")}
          detailsNode={
            // The plan verbatim — these are exactly what confirm will run.
            <div className="flex flex-col gap-1.5">
              {confirmRequest.plan.commands.map((command, index) => (
                <code
                  key={`${index}-${command}`}
                  className="block break-all rounded-md bg-background px-2 py-1 font-mono text-[12px] leading-4 text-secondary"
                >
                  {command}
                </code>
              ))}
              {confirmRequest.plan.latest_version && (
                <span className="text-[12px] text-muted">
                  {t("mcp.remoteVersion", { version: confirmRequest.plan.latest_version })}
                </span>
              )}
            </div>
          }
          onClose={() => setConfirmRequest(null)}
          onConfirm={() => runUpgrade(confirmRequest.server)}
        />
      )}

      {confirmRequest?.kind === "delete" && (
        <ConfirmDialog
          open
          tone="danger"
          title={t("mcp.deleteServer")}
          message={t("mcp.deleteConfirmBody", { name: confirmRequest.server.name })}
          details={
            confirmRequest.server.bindings.length > 0
              ? confirmRequest.server.bindings.map((binding) => {
                  const path = report?.agents.find(
                    (agent) => agent.agent_key === binding.agent_key,
                  )?.config_path;
                  return `${agentName(binding.agent_key)}${path ? ` · ${path}` : ""}`;
                })
              : [t("mcp.deleteNoBindings")]
          }
          onClose={() => setConfirmRequest(null)}
          onConfirm={() => runDelete(confirmRequest.server)}
        />
      )}
    </div>
  );
}