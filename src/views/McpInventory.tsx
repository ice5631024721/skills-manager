import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
  Share2,
  Square,
  SquareCheck,
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
import { MultiSelectToolbar } from "../components/MultiSelectToolbar";
import { useMultiSelect } from "../hooks/useMultiSelect";
import { cn } from "../utils";
import { getErrorMessage } from "../lib/error";
import * as api from "../lib/tauri";
import type {
  McpAgentStatus,
  McpInventoryReport,
  McpServerDto,
  McpServerOccurrence,
  McpServerSummary,
  PendingDrift,
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

/** The command string an agent's copy actually runs. */
function occurrenceEndpoint(occurrence: McpServerOccurrence): string {
  return occurrence.command || occurrence.url || "";
}

/** The takeover loop distinguishes "claim in place" from "differs" by the
 *  backend's error text (commit 22933c5): equivalents are claimed silently,
 *  divergent copies say so and defer to the overwrite confirmation. */
function isDiffersError(err: unknown): boolean {
  return getErrorMessage(err, "").includes("differs");
}

// ── Managed library (ADR-0006) + inventory, merged by name ──

/** One card per server NAME: the union of library records and inventory
 *  summaries. A card is managed iff a library record exists for the name. */
type ManagedCard = { kind: "managed"; name: string; server: McpServerDto; summary: McpServerSummary | null };
type ForeignCard = { kind: "foreign"; name: string; summary: McpServerSummary };
type McpCard = ManagedCard | ForeignCard;

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

interface AgentPickRow {
  agentKey: string;
  displayName: string;
  sub: string;
}

interface AgentPickDialogProps {
  title: string;
  message: string;
  rows: AgentPickRow[];
  onPick: (agentKey: string) => void;
  onClose: () => void;
}

/** ConfirmDialog-styled one-of-N list over agents. Used both to pick which
 *  agent's copy becomes the managed definition (takeover) and which agent a
 *  batch sync targets. */
function AgentPickDialog({ title, message, rows, onPick, onClose }: AgentPickDialogProps) {
  const { t } = useTranslation();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={onClose} />
      <div className="relative flex max-h-[calc(85vh/var(--app-scale))] w-full max-w-sm flex-col rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-2 flex shrink-0 items-center justify-between">
          <h2 className="flex min-w-0 items-center gap-2 text-[13px] font-semibold text-primary">
            <Plug className="h-4 w-4 shrink-0 text-accent-light" />
            <span className="truncate">{title}</span>
          </h2>
          <button
            onClick={onClose}
            aria-label={t("common.cancel")}
            className="rounded p-1 text-muted outline-none transition-colors hover:text-secondary"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
        <p className="mb-3 shrink-0 text-[13px] leading-5 text-tertiary">{message}</p>
        <div className="min-h-0 flex-1 space-y-1 overflow-y-auto">
          {rows.map((row) => (
            <button
              key={row.agentKey}
              type="button"
              onClick={() => onPick(row.agentKey)}
              className="flex w-full items-center gap-2.5 rounded-lg px-2 py-2 text-left outline-none transition-colors hover:bg-surface-hover focus-visible:ring-2 focus-visible:ring-accent"
            >
              <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg border border-border-subtle bg-surface-hover text-secondary">
                <AgentIcon
                  agentKey={row.agentKey}
                  displayName={row.displayName}
                  className="h-4 w-4"
                />
              </span>
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="truncate text-[13px] font-medium text-primary">
                  {row.displayName}
                </span>
                <span className="truncate font-mono text-[11px] text-muted" title={row.sub}>
                  {row.sub}
                </span>
              </span>
            </button>
          ))}
        </div>
      </div>
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
  // Takeover pickers and batch flows (single grid, one card per name).
  const [takeoverPick, setTakeoverPick] = useState<{
    name: string;
    occurrences: McpServerOccurrence[];
  } | null>(null);
  const [batchSyncPickOpen, setBatchSyncPickOpen] = useState(false);
  const [batchDeleteOpen, setBatchDeleteOpen] = useState(false);
  // (card name, agent) whose dot is mid-write; and the card whose takeover
  // loop is running; and whether a batch action is running.
  const [busyDot, setBusyDot] = useState<{ card: string; agent: string } | null>(null);
  const [busyProbe, setBusyProbe] = useState<string | null>(null);
  const [busyUpgrade, setBusyUpgrade] = useState<string | null>(null);
  const [busyTakeoverName, setBusyTakeoverName] = useState<string | null>(null);
  const [batchBusy, setBatchBusy] = useState(false);

  /** THE reload: inventory + library in one shot. Every mutation and the
   *  rescan button go through here, so the two data sources can never
   *  disagree on screen (the old bug: mutations re-pulled only the library). */
  const load = useCallback(
    async (initial: boolean) => {
      if (initial) {
        setLoading(true);
        setError(null);
      }
      try {
        const [inventory, library] = await Promise.all([
          api.getMcpInventory(),
          api.getMcpLibrary(),
        ]);
        setReport(inventory);
        setLibraryServers(library.servers);
      } catch (err) {
        const message = getErrorMessage(err, t("common.error"));
        if (initial) setError(message);
        else toast.error(t("mcp.refreshFailed", { message }));
      } finally {
        if (initial) setLoading(false);
      }
    },
    [t],
  );

  /** Silent re-pull after any write/probe; errors surface as toasts so the
   *  triggering action's outcome is never hidden behind a reload. */
  const refreshAll = useCallback(() => void load(false), [load]);

  /** Non-blocking probe whose result lands in state; the follow-up refresh
   *  makes "did it really take effect" visible without a manual rescan. */
  const probeQuiet = useCallback(
    (serverId: string) => {
      void api
        .probeMcpServer(serverId)
        .catch(() => undefined)
        .finally(() => load(false));
    },
    [load],
  );

  useEffect(() => {
    void load(true);
  }, [load]);

  // Startup upstream round, mirroring the skill auto-update one: fires once
  // per mount after the first successful load, non-blocking, then re-pulls
  // everything. The ref survives `loading`/`error` edges (that edge cycling
  // is what recently caused a toast loop); there is deliberately no interval
  // timer — periodic rounds are the Rust scheduler's job.
  const updateRoundRef = useRef(false);
  useEffect(() => {
    if (loading || error || updateRoundRef.current) return;
    updateRoundRef.current = true;
    void (async () => {
      try {
        await api.checkMcpUpdates(false);
        await load(false);
      } catch {
        // Silent: badges stay stale and the next rescan retries.
      }
    })();
  }, [loading, error, load]);

  const agents = useMemo(() => report?.agents ?? [], [report]);

  const agentName = useCallback(
    (agentKey: string) =>
      agents.find((agent) => agent.agent_key === agentKey)?.display_name ?? agentKey,
    [agents],
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

  const dialogsBusy =
    addDialog !== null ||
    confirmRequest !== null ||
    driftChain !== null ||
    takeoverPick !== null ||
    batchSyncPickOpen ||
    batchDeleteOpen;

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

  // ── the single grid: library ∪ inventory, joined by name ──

  const summaryByName = useMemo(
    () => new Map((report?.servers ?? []).map((summary) => [summary.name, summary])),
    [report],
  );

  const cards = useMemo<McpCard[]>(() => {
    const libraryNames = new Set(libraryServers.map((server) => server.name));
    const managed: McpCard[] = libraryServers.map((server) => ({
      kind: "managed" as const,
      name: server.name,
      server,
      summary: summaryByName.get(server.name) ?? null,
    }));
    const foreign: McpCard[] = (report?.servers ?? [])
      .filter((summary) => !libraryNames.has(summary.name))
      .map((summary) => ({ kind: "foreign" as const, name: summary.name, summary }));
    return [...managed, ...foreign].sort((a, b) => a.name.localeCompare(b.name));
  }, [libraryServers, summaryByName, report]);

  // ── multi-select (mirrors MySkills; keyed by card name) ──

  const {
    isMultiSelect,
    setIsMultiSelect,
    selectedIds,
    toggleSelect,
    isAllSelected,
    handleSelectAll,
    exitMultiSelect,
  } = useMultiSelect<McpCard>({
    items: cards,
    filtered: cards,
    getKey: (card) => card.name,
    isItemActive: () => true,
    filterSignal: "mcp-cards",
    escapeEnabled: !dialogsBusy,
  });

  const selectedForeign = useMemo(
    () =>
      cards.filter(
        (card): card is ForeignCard => card.kind === "foreign" && selectedIds.has(card.name),
      ),
    [cards, selectedIds],
  );
  const selectedManaged = useMemo(
    () =>
      cards.filter(
        (card): card is ManagedCard => card.kind === "managed" && selectedIds.has(card.name),
      ),
    [cards, selectedIds],
  );

  // ── per-(server, agent) dot writes ──

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
      setBusyDot({ card: server.name, agent: agentKey });
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
          // Foreign same-name collisions land here too (commit 22933c5).
          requestDriftChain({
            name: server.name,
            queue: [outcome],
            retry: write,
            agentLabel: agentName,
            onDone: ({ cancelled }) => {
              if (cancelled === null) {
                syncToast(server, agentKey, nextDesired);
                if (nextDesired) probeQuiet(server.id);
              } else {
                toast.info(t("mcp.driftStopped", { name: server.name }));
              }
              refreshAll();
            },
          });
          return;
        }
        syncToast(server, agentKey, nextDesired);
        if (nextDesired) probeQuiet(server.id);
        refreshAll();
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
        refreshAll();
      } finally {
        setBusyDot(null);
      }
    },
    [agentName, probeQuiet, refreshAll, requestDriftChain, syncToast, t],
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
        refreshAll();
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
      } finally {
        setBusyProbe(null);
      }
    },
    [refreshAll, t],
  );

  // ── takeover (ADR-0006 §7, single-grid edition) ──

  /** Adopt one agent's copy as the managed definition. */
  const takeoverSingle = useCallback(
    async (occurrence: McpServerOccurrence, name: string) => {
      setBusyDot({ card: name, agent: occurrence.agent_key });
      try {
        await api.takeoverMcpEntry(occurrence.agent_key, name);
        toast.success(t("mcp.takeoverDone", { name }), {
          description: occurrence.agent_display_name,
        });
      } catch (err) {
        toast.error(t("mcp.takeoverFailed", { name }), {
          description: getErrorMessage(err, t("common.error")),
        });
      } finally {
        setBusyDot(null);
        await load(false);
      }
    },
    [load, t],
  );

  /** Card-level takeover: `first` creates the record, every other agent is
   *  then claimed in place — except copies that read back different, which
   *  keep their foreign-here dot and resolve through the overwrite chain. */
  const runTakeoverLoop = useCallback(
    async (name: string, occurrences: McpServerOccurrence[], firstAgent: string): Promise<boolean> => {
      setBusyTakeoverName(name);
      try {
        try {
          await api.takeoverMcpEntry(firstAgent, name);
        } catch (err) {
          toast.error(t("mcp.takeoverFailed", { name }), {
            description: getErrorMessage(err, t("common.error")),
          });
          return false;
        }
        let differs = 0;
        for (const occurrence of occurrences) {
          if (occurrence.agent_key === firstAgent) continue;
          try {
            await api.takeoverMcpEntry(occurrence.agent_key, name);
          } catch (err) {
            if (isDiffersError(err)) differs += 1;
            else toast.error(t("mcp.takeoverFailed", { name }), {
              description: getErrorMessage(err, t("common.error")),
            });
          }
        }
        toast.success(t("mcp.takeoverDone", { name }));
        if (differs > 0) toast.warning(t("mcp.takeoverDiffers", { name }));
        return true;
      } finally {
        setBusyTakeoverName(null);
        await load(false);
      }
    },
    [load, t],
  );

  const handleTakeoverCard = useCallback(
    (summary: McpServerSummary) => {
      const occurrences = summary.agents;
      if (occurrences.length === 0) return;
      const commands = new Set(occurrences.map(occurrenceEndpoint));
      if (commands.size <= 1) {
        void runTakeoverLoop(summary.name, occurrences, occurrences[0].agent_key);
        return;
      }
      setTakeoverPick({ name: summary.name, occurrences });
    },
    [runTakeoverLoop],
  );

  const handleBatchTakeover = useCallback(async () => {
    setBatchBusy(true);
    let done = 0;
    try {
      for (const card of selectedForeign) {
        const occurrences = card.summary.agents;
        if (occurrences.length === 0) continue;
        // Divergent copies in a batch take the first occurrence as the
        // definition of record; the rest claim in place or keep their
        // foreign-here dot for a later overwrite.
        if (await runTakeoverLoop(card.name, occurrences, occurrences[0].agent_key)) done += 1;
      }
    } finally {
      setBatchBusy(false);
    }
    // Nothing landed? Keep the selection so the batch can be retried.
    if (done > 0) exitMultiSelect();
  }, [selectedForeign, runTakeoverLoop, exitMultiSelect]);

  const runBatchSyncTo = useCallback(
    async (agentKey: string) => {
      setBatchSyncPickOpen(false);
      const targets = selectedManaged;
      if (targets.length === 0) return;
      setBatchBusy(true);
      const landed: McpServerDto[] = [];
      const drifts: { server: McpServerDto; pending: PendingDrift }[] = [];
      const byToken = new Map<string, McpServerDto>();
      for (const card of targets) {
        try {
          const outcome = await api.syncMcpToAgent(card.server.id, agentKey);
          if (outcome.status === "applied") landed.push(card.server);
          else {
            drifts.push({ server: card.server, pending: outcome });
            byToken.set(outcome.token, card.server);
          }
        } catch (err) {
          toast.error(t("mcp.syncedToAgentFailed", { name: card.name }), {
            description: getErrorMessage(err, t("common.error")),
          });
        }
      }
      const finish = (doneServers: McpServerDto[], cancelledCount: number) => {
        doneServers.forEach((server) => probeQuiet(server.id));
        load(false);
        setBatchBusy(false);
        if (doneServers.length > 0) {
          toast.success(
            t("mcp.batchSyncToDone", { count: doneServers.length, agent: agentName(agentKey) }),
          );
        }
        if (cancelledCount > 0) {
          toast.info(t("mcp.batchSyncToSkipped", { count: cancelledCount }));
        }
        exitMultiSelect();
      };
      if (drifts.length === 0) {
        finish(landed, 0);
        return;
      }
      requestDriftChain({
        name: drifts.map((d) => d.server.name).join(", "),
        queue: drifts.map((d) => d.pending),
        retry: (token) => {
          const server = byToken.get(token);
          if (!server) return Promise.reject(new Error(`unknown drift token`));
          return api.syncMcpToAgent(server.id, agentKey, token).then((outcome) => {
            // Fresh tokens from the replay still belong to this server.
            if (outcome.status === "pending_drift") byToken.set(outcome.token, server);
            return outcome;
          });
        },
        agentLabel: agentName,
        onDone: ({ cancelled }) => {
          if (cancelled === null) {
            finish([...landed, ...drifts.map((d) => d.server)], 0);
          } else {
            const cancelledTokens = new Set(cancelled.map((d) => d.token));
            const approved = drifts.filter((d) => !cancelledTokens.has(d.pending.token));
            finish(
              [...landed, ...approved.map((d) => d.server)],
              cancelled.length,
            );
          }
        },
      });
    },
    [agentName, exitMultiSelect, load, probeQuiet, requestDriftChain, selectedManaged, t],
  );

  const runBatchDelete = useCallback(async () => {
    setBatchDeleteOpen(false);
    const targets = selectedManaged;
    if (targets.length === 0) return;
    setBatchBusy(true);
    const deleted: McpServerDto[] = [];
    const drifts: { server: McpServerDto; list: PendingDrift[] }[] = [];
    const byToken = new Map<string, string>();
    for (const card of targets) {
      try {
        const outcome = await api.deleteMcpServer(card.server.id);
        if (outcome.pending_drift.length === 0) deleted.push(card.server);
        else {
          drifts.push({ server: card.server, list: outcome.pending_drift });
          for (const pending of outcome.pending_drift) {
            byToken.set(pending.token, card.server.id);
          }
        }
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
      }
    }
    const settle = (done: number, kept: number) => {
      load(false);
      setBatchBusy(false);
      if (done > 0) toast.success(t("mcp.batchDeleteDone", { count: done }));
      if (kept > 0) toast.info(t("mcp.batchDeleteKept", { count: kept }));
      exitMultiSelect();
    };
    if (drifts.length === 0) {
      settle(deleted.length, 0);
      return;
    }
    requestDriftChain({
      name: drifts.map((d) => d.server.name).join(", "),
      queue: drifts.flatMap((d) => d.list),
      retry: (token) => {
        const serverId = byToken.get(token);
        if (!serverId) return Promise.reject(new Error(`unknown drift token`));
        return api.deleteMcpServer(serverId, token).then((outcome) => {
          for (const pending of outcome.pending_drift) {
            byToken.set(pending.token, serverId);
          }
          return outcome;
        });
      },
      agentLabel: agentName,
      onDone: ({ cancelled }) => {
        if (cancelled === null) {
          settle(deleted.length + drifts.length, 0);
        } else {
          const cancelledTokens = new Set(cancelled.map((d) => d.token));
          const kept = drifts.filter((d) =>
            d.list.some((pending) => cancelledTokens.has(pending.token)),
          ).length;
          settle(deleted.length + drifts.length - kept, kept);
        }
      },
    });
  }, [agentName, exitMultiSelect, load, requestDriftChain, selectedManaged, t]);

  // ── confirm-request runners (single managed card) ──

  const runUpgrade = useCallback(
    async (server: McpServerDto) => {
      try {
        await api.applyMcpUpgrade(server.id);
        toast.success(t("mcp.upgradeDone", { name: server.name }), {
          description: t("mcp.upgradeReconnectHint"),
        });
        probeQuiet(server.id);
      } catch (err) {
        toast.error(getErrorMessage(err, t("common.error")));
      } finally {
        load(false);
      }
    },
    [load, probeQuiet, t],
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
              load(false);
            },
          });
          return;
        }
        toast.success(t("mcp.deleteDone", { name: server.name }));
        load(false);
      } catch (err) {
        // Mirrors the skill removal loop: report, then re-pull so the UI
        // shows whatever the refused call really left behind.
        toast.error(getErrorMessage(err, t("common.error")));
        load(false);
      }
    },
    [agentName, load, requestDriftChain, t],
  );

  // ── section renderers ──

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

  /** Checkbox affordance, MySkills-style: always on in select mode, a hover
   *  reveal otherwise; top-right of the card. */
  const renderSelectBox = (card: McpCard) => (
    <button
      type="button"
      aria-label={card.name}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        if (!isMultiSelect) setIsMultiSelect(true);
        toggleSelect(card.name);
      }}
      className={cn(
        "absolute right-2 top-2 z-10 flex h-5 w-5 items-center justify-center rounded-md border border-border bg-surface/95 shadow-sm outline-none transition-opacity hover:border-accent-border focus-visible:ring-2 focus-visible:ring-accent",
        isMultiSelect ? "opacity-100" : "opacity-0 group-hover:opacity-100"
      )}
    >
      {isMultiSelect && selectedIds.has(card.name) ? (
        <SquareCheck className="h-3.5 w-3.5 text-accent" />
      ) : (
        <Square className="h-3.5 w-3.5 text-faint" />
      )}
    </button>
  );

  const cardShellClass = (card: McpCard) =>
    cn(
      "app-panel group relative flex h-full flex-col shadow-card transition-all hover:-translate-y-px hover:border-border hover:shadow-card-hover",
      isMultiSelect && "cursor-pointer",
      isMultiSelect && selectedIds.has(card.name) && "ring-1 ring-accent border-accent/40"
    );

  const actionClass =
    "rounded-md p-1.5 text-muted transition-colors hover:bg-surface-hover hover:text-primary disabled:opacity-50";

  const renderEnvChips = (keys: string[]) =>
    keys.length > 0 && (
      <div className="mt-2 flex flex-wrap items-center gap-1">
        {keys.map((key) => (
          <span
            key={key}
            className="inline-flex items-center rounded-full border border-border-subtle bg-surface-hover px-2 py-0.5 font-mono text-[11px] text-muted"
          >
            {key}
          </span>
        ))}
        <span className="text-[11px] text-faint">{t("mcp.envKeysOnly")}</span>
      </div>
    );

  const renderEndpointLine = (endpoint: string, isCommand: boolean) =>
    endpoint ? (
      <div className="flex items-center gap-1.5 text-[12px] text-muted" title={endpoint}>
        {isCommand ? (
          <Terminal className="h-3.5 w-3.5 shrink-0 text-faint" />
        ) : (
          <Globe className="h-3.5 w-3.5 shrink-0 text-faint" />
        )}
        <span className="truncate font-mono">{endpoint}</span>
      </div>
    ) : (
      <div className="text-[12px] text-faint">{t("mcp.noEndpoint")}</div>
    );

  const renderManagedCard = (card: ManagedCard) => {
    const { server, summary } = card;
    const endpoint = server.command
      ? [server.command, ...server.args].join(" ")
      : server.url ?? "";
    // Key names only, never values (ADR-0006: env values stay masked).
    const envKeys = Object.keys(server.env);
    const hasUpdate = server.update_status === "update_available";
    const isProbing = busyProbe === server.id;
    // Foreign copies of this same name, straight from the inventory: they
    // turn otherwise-absent dots into dashed foreign-here dots.
    const presence = new Set((summary?.agents ?? []).map((o) => o.agent_key));
    const pendingAgent = busyDot && busyDot.card === server.name ? busyDot.agent : null;

    return (
      <div
        key={card.name}
        className={cardShellClass(card)}
        onClick={isMultiSelect ? () => toggleSelect(card.name) : undefined}
      >
        {renderSelectBox(card)}
        <div className="flex items-center gap-2 px-3.5 pt-3 pb-1.5">
          <ProbeIndicator server={server} />
          <h3
            className="flex-1 truncate text-[14px] font-semibold text-primary group-hover:text-accent-light"
            title={server.name}
          >
            {server.name}
          </h3>
          <div className={cn("flex shrink-0 items-center gap-2", isMultiSelect && "hidden")}>
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
        </div>

        <div className="px-3.5 pb-3">
          {renderEndpointLine(endpoint, !!server.command)}
          {renderEnvChips(envKeys)}
        </div>

        <div className="mt-auto flex items-center justify-between gap-2 border-t border-border-faint px-3.5 py-2.5">
          <McpAgentDots
            agents={agents}
            bindings={server.bindings}
            presence={presence}
            size="sm"
            onToggle={
              isMultiSelect
                ? undefined
                : (agentKey, nextDesired) => void handleToggle(server, agentKey, nextDesired)
            }
            pendingKey={pendingAgent}
          />
          {!isMultiSelect && (
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
          )}
        </div>
      </div>
    );
  };

  const renderForeignCard = (card: ForeignCard) => {
    const { summary } = card;
    const endpoint = summary.command || summary.url;
    // The visible wording is uniform ("registered") because only some formats
    // carry an explicit on/off field; the switch state stays in the tooltip.
    const nativeState = (occurrence: McpServerOccurrence) =>
      occurrence.enabled === null
        ? t("mcp.state.registered")
        : occurrence.enabled
          ? t("mcp.state.enabled")
          : t("mcp.state.disabled");
    const states = summary.agents.map(nativeState);
    const allDisabled =
      summary.agents.length > 0 &&
      summary.agents.every((occurrence) => occurrence.enabled === false);
    const configPaths = Array.from(new Set(summary.agents.map((o) => o.config_path)));
    // Per-agent endpoints only earn a line when they actually differ; a merged
    // server with identical commands would just repeat the line above.
    const distinctEndpoints = new Set(summary.agents.map(occurrenceEndpoint));
    const presence = new Set(summary.agents.map((o) => o.agent_key));
    const pendingAgent = busyDot && busyDot.card === card.name ? busyDot.agent : null;
    const takingOver = busyTakeoverName === card.name;

    return (
      <div
        key={card.name}
        className={cardShellClass(card)}
        onClick={isMultiSelect ? () => toggleSelect(card.name) : undefined}
      >
        {renderSelectBox(card)}
        {takingOver && (
          <div className="absolute inset-0 z-20 flex items-center justify-center rounded-xl bg-surface/70 backdrop-blur-[1px]">
            <Loader2 className="h-5 w-5 animate-spin text-muted" />
          </div>
        )}
        <div className="flex items-center gap-2.5 px-3.5 pt-3 pb-1.5">
          <span
            className={cn(
              "h-2 w-2 shrink-0 rounded-full transition-opacity",
              allDisabled
                ? "bg-surface-active"
                : "bg-accent-light shadow-[0_0_0_3px_var(--color-accent-bg)]"
            )}
            title={states.join(" · ")}
          />
          <h3
            className="flex-1 truncate text-[14px] font-semibold text-primary group-hover:text-accent-light"
            title={summary.name}
          >
            {summary.name}
          </h3>
          <span
            className={cn(
              "inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[11px] font-medium",
              transportClassName(summary.transport),
              isMultiSelect && "hidden"
            )}
          >
            {summary.transport}
          </span>
        </div>

        <div className="px-3.5 pb-3">
          {renderEndpointLine(endpoint || "", !!summary.command)}
          {renderEnvChips(summary.env_keys)}

          {distinctEndpoints.size > 1 && (
            <div className="mt-2 space-y-1 border-t border-border-faint pt-2">
              {summary.agents.map((occurrence) => (
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
                    {occurrenceEndpoint(occurrence) || t("mcp.noEndpoint")}
                  </span>
                </div>
              ))}
            </div>
          )}

          <div
            className="mt-2 flex min-w-0 items-center gap-1 text-[12px] text-muted"
            title={configPaths.join("\n")}
          >
            <ScrollText className="h-3 w-3 shrink-0" />
            <span className="truncate">
              {configPaths.length === 1
                ? fileName(configPaths[0])
                : t("mcp.configFiles", { count: configPaths.length })}
            </span>
          </div>
        </div>

        <div className="mt-auto flex items-center justify-between gap-2 border-t border-border-faint px-3.5 py-2.5">
          <McpAgentDots
            agents={agents}
            bindings={[]}
            presence={presence}
            variant="foreign"
            size="sm"
            onTakeover={
              isMultiSelect
                ? undefined
                : (agentKey) => {
                    const occurrence = summary.agents.find((o) => o.agent_key === agentKey);
                    if (occurrence) void takeoverSingle(occurrence, card.name);
                  }
            }
            pendingKey={pendingAgent}
          />
          {!isMultiSelect && (
            <button
              type="button"
              disabled={takingOver}
              onClick={() => handleTakeoverCard(summary)}
              className="inline-flex shrink-0 items-center gap-1.5 rounded-md border border-border-subtle px-2 py-1 text-[12px] font-medium text-secondary outline-none transition-colors hover:border-accent-border hover:bg-surface-hover focus-visible:ring-2 focus-visible:ring-accent disabled:opacity-50"
            >
              {takingOver ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <ArrowDownToLine className="h-3.5 w-3.5" />
              )}
              {t("mcp.takeover")}
            </button>
          )}
        </div>
      </div>
    );
  };

  const serverCount = cards.length;
  const agentCount = report?.agents.filter((agent) => agent.server_count > 0).length ?? 0;
  const installedAgents = agents.filter((agent) => agent.installed);

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
              {t("mcp.summary", { servers: report.servers.length, agents: agentCount })}
            </span>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <button
            type="button"
            aria-pressed={isMultiSelect}
            onClick={() => (isMultiSelect ? exitMultiSelect() : setIsMultiSelect(true))}
            className={cn(
              "app-segmented-button inline-flex h-10 items-center gap-1.5 hover:bg-surface-hover focus-visible:ring-2 focus-visible:ring-border",
              isMultiSelect &&
                "app-segmented-button-active hover:bg-surface-active hover:text-secondary"
            )}
          >
            <SquareCheck className="h-4 w-4" />
            {isMultiSelect ? t("mcp.cancelSelect") : t("mcp.selectMode")}
          </button>
          <button
            type="button"
            onClick={openAddDialog}
            className="app-toolbar-button app-toolbar-button-secondary"
          >
            <Plus className="h-3.5 w-3.5" />
            {t("mcp.addServer")}
          </button>
          <button
            type="button"
            onClick={() => void load(true)}
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
            <div className="grid gap-3 sm:grid-cols-2">{agents.map(renderAgentRow)}</div>
          </div>

          {isMultiSelect && (
            <MultiSelectToolbar
              selectedCount={selectedIds.size}
              isAllSelected={isAllSelected}
              actions={[
                {
                  key: "takeover",
                  tone: selectedForeign.length > 0 ? "primary" : "secondary",
                  label: t("mcp.batchTakeover", { count: selectedForeign.length }),
                  icon: <ArrowDownToLine className="h-3.5 w-3.5" />,
                  busy: batchBusy,
                  disabled: selectedForeign.length === 0,
                  onSelect: () => void handleBatchTakeover(),
                },
                {
                  key: "sync",
                  label: t("mcp.batchSyncTo", { count: selectedManaged.length }),
                  icon: <Share2 className="h-3.5 w-3.5" />,
                  busy: batchBusy,
                  disabled: selectedManaged.length === 0,
                  onSelect: () => setBatchSyncPickOpen(true),
                },
                {
                  key: "delete",
                  tone: "danger",
                  label: t("mcp.batchDelete", { count: selectedManaged.length }),
                  icon: <Trash2 className="h-3.5 w-3.5" />,
                  busy: batchBusy,
                  disabled: selectedManaged.length === 0,
                  onSelect: () => setBatchDeleteOpen(true),
                },
              ]}
              labels={{
                hint: t("mcp.selectHint"),
                selected: t("mcp.selectedCount", { count: selectedIds.size }),
                selectAll: t("mcp.selectAll"),
                deselectAll: t("mcp.deselectAll"),
                cancel: t("common.cancel"),
                more: t("mcp.moreActions"),
              }}
              onSelectAll={handleSelectAll}
              onCancel={exitMultiSelect}
            />
          )}

          {cards.length === 0 ? (
            <div className="app-panel border-dashed px-4 py-8 text-center text-[13px] text-muted">
              {t("mcp.libraryEmpty")}
            </div>
          ) : (
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-3">
              {cards.map((card) =>
                card.kind === "managed" ? renderManagedCard(card) : renderForeignCard(card)
              )}
            </div>
          )}
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
          onChanged={refreshAll}
          runDriftChain={requestDriftChain}
          agentLabel={agentName}
        />
      )}

      {driftChain && <McpDriftDialog chain={driftChain} />}

      {takeoverPick && (
        <AgentPickDialog
          title={t("mcp.takeoverPick")}
          message={t("mcp.takeoverPickBody", { name: takeoverPick.name })}
          rows={takeoverPick.occurrences.map((occurrence) => ({
            agentKey: occurrence.agent_key,
            displayName: occurrence.agent_display_name,
            sub: occurrenceEndpoint(occurrence) || t("mcp.noEndpoint"),
          }))}
          onPick={(agentKey) => {
            const { name, occurrences } = takeoverPick;
            setTakeoverPick(null);
            void runTakeoverLoop(name, occurrences, agentKey);
          }}
          onClose={() => setTakeoverPick(null)}
        />
      )}

      {batchSyncPickOpen && (
        <AgentPickDialog
          title={t("mcp.batchSyncToPick")}
          message={t("mcp.batchSyncToBody", { count: selectedManaged.length })}
          rows={installedAgents.map((agent) => ({
            agentKey: agent.agent_key,
            displayName: agent.display_name,
            sub: agent.config_path,
          }))}
          onPick={(agentKey) => void runBatchSyncTo(agentKey)}
          onClose={() => setBatchSyncPickOpen(false)}
        />
      )}

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
                  const path = agents.find(
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

      {batchDeleteOpen && (
        <ConfirmDialog
          open
          tone="danger"
          title={t("mcp.batchDelete", { count: selectedManaged.length })}
          message={t("mcp.batchDeleteConfirmBody", { count: selectedManaged.length })}
          details={selectedManaged.map((card) => card.name)}
          confirmLabel={t("common.delete")}
          onClose={() => setBatchDeleteOpen(false)}
          onConfirm={() => runBatchDelete()}
        />
      )}
    </div>
  );
}
