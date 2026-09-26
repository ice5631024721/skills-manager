import { useEffect, useRef, useState } from "react";
import { AlertTriangle, Check, Loader2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { getErrorMessage } from "../lib/error";
import type { EditOutcome, PendingDrift, WriteOutcome } from "../lib/tauri";
import { cn } from "../utils";

/** What a drift-approved replay returns: either the single-agent outcome or
 *  the multi-agent one (edit/delete), both of which can carry FRESH pending
 *  drifts — a token minted before the file changed again is rejected with a
 *  new one, so the chain always replays with the freshest token only. */
export type DriftRetryResult = WriteOutcome | EditOutcome;

/** One confirmation chain: an operation refuses to touch drifted agent files
 *  until the user approves each overwrite (ADR-0006 §4). The backend consumes
 *  ONE token per call, so agents are confirmed one dialog at a time; refusing
 *  anywhere ends the chain and leaves the remaining agents untouched
 *  (already-applied ones stay written — the ledger never lies). */
export interface McpDriftChain {
  /** Definition name shown in the header. */
  name: string;
  /** Pending drifts to work through, one at a time. */
  queue: PendingDrift[];
  /** Agents the operation already applied before the first refusal. */
  applied?: string[];
  /** Replay the originating operation with one approved token. */
  retry: (token: string) => Promise<DriftRetryResult>;
  /** Human label for an agent key (display name). */
  agentLabel?: (agentKey: string) => string;
  /** Called exactly once when the chain settles. `cancelled` lists the
   *  drifts that were never approved; `null` means everything landed. */
  onDone: (result: { applied: string[]; cancelled: PendingDrift[] | null }) => void;
}

/** Guard against a pathological loop (file moving under every retry). */
const MAX_ROUNDS = 10;

function isEditOutcome(result: DriftRetryResult): result is EditOutcome {
  return "applied" in result;
}

/** The agents a replay reported as written. */
function landedAgents(result: DriftRetryResult, viaAgent: string): string[] {
  if (isEditOutcome(result)) return result.applied;
  return result.status === "applied" ? [viaAgent] : [];
}

/** The agents still waiting on a confirmation after a replay. */
function pendingAgents(result: DriftRetryResult): PendingDrift[] {
  if (isEditOutcome(result)) return result.pending_drift;
  return result.status === "pending_drift" ? [result] : [];
}

interface Props {
  chain: McpDriftChain;
}

/** ConfirmDialog-style modal showing current_text vs planned_text for one
 *  agent at a time; confirm = overwrite that agent and continue the chain. */
export function McpDriftDialog({ chain }: Props) {
  const { t } = useTranslation();
  const [queue, setQueue] = useState<PendingDrift[]>(chain.queue);
  const [applied, setApplied] = useState<string[]>(chain.applied ?? []);
  const [busy, setBusy] = useState(false);
  const [rounds, setRounds] = useState(0);
  const doneRef = useRef(false);
  const head = queue[0];

  // Exactly-once: the parent unmounts on the first call, but a click can
  // land in the same tick as an approve finishing.
  const settle = (done: { applied: string[]; cancelled: PendingDrift[] | null }) => {
    if (doneRef.current) return;
    doneRef.current = true;
    chain.onDone(done);
  };

  const cancel = () => {
    if (busy) return;
    settle({ applied, cancelled: queue });
  };

  // Escape cancels the rest of the chain (but never mid-replay).
  useEffect(() => {
    if (busy) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") cancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [busy, queue, applied]);

  const approve = async () => {
    if (!head || busy) return;
    setBusy(true);
    try {
      const result = await chain.retry(head.token);
      const landed = landedAgents(result, head.agent_key);
      // Set-merge: an EditOutcome re-reports every already-clean agent on
      // each replay, and a stale token re-queues its own agent with a fresh one.
      const nextApplied = Array.from(new Set([...applied, ...landed]));
      // Carry the rest of the queue, then merge the replay's answer,
      // deduping by TOKEN (not agent): tokens are deterministic per
      // (agent, name, ledger fp, revision), so an EditOutcome replay that
      // re-reports a still-waiting agent produces the SAME token already
      // queued — the Map refreshes it instead of duplicating the step (the
      // original F1 bug) — while different servers sharing one agent_key
      // (batch sync) or spanning servers (batch delete) keep distinct
      // tokens and survive (the by-agent rebuild dropped them).
      const answered = pendingAgents(result);
      const byToken = new Map<string, PendingDrift>();
      for (const pending of [...queue.slice(1), ...answered]) {
        byToken.set(pending.token, pending);
      }
      const next = Array.from(byToken.values());
      const round = rounds + 1;
      setRounds(round);
      setApplied(nextApplied);
      if (next.length === 0) {
        settle({ applied: nextApplied, cancelled: null });
      } else if (round >= MAX_ROUNDS) {
        toast.error(t("mcp.driftAborted", { name: chain.name }));
        settle({ applied: nextApplied, cancelled: next });
      } else {
        setQueue(next);
      }
    } catch (err) {
      toast.error(getErrorMessage(err, t("common.error")));
      settle({ applied, cancelled: queue });
    } finally {
      setBusy(false);
    }
  };

  if (!head) return null;

  const agentName = chain.agentLabel?.(head.agent_key) ?? head.agent_key;
  const total = applied.length + queue.length;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={cancel} />
      {/* Capped like ConfirmDialog (#430): the vh cap divides by --app-scale
          because `zoom` on <html> does not scale vh units. */}
      <div className="relative flex max-h-[calc(85vh/var(--app-scale))] w-full max-w-2xl flex-col rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-4 flex shrink-0 items-center justify-between">
          <h2 className="flex min-w-0 items-center gap-2 text-[13px] font-semibold text-primary">
            <AlertTriangle className="h-4 w-4 shrink-0 text-amber-400" />
            <span className="truncate">
              {head.kind === "foreign" ? t("mcp.foreignOverwriteTitle") : t("mcp.driftTitle")}
            </span>
            <span className="shrink-0 rounded-full bg-surface-hover px-2 py-0.5 text-[11px] font-medium text-muted">
              {t("mcp.driftStep", { current: Math.min(applied.length + 1, total), total })}
            </span>
          </h2>
          <button
            onClick={cancel}
            disabled={busy}
            aria-label={t("common.cancel")}
            className="rounded p-1 text-muted outline-none transition-colors hover:text-secondary disabled:opacity-50"
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        <p className="mb-4 shrink-0 text-[13px] leading-5 text-tertiary">
          {head.kind === "foreign"
            ? t("mcp.foreignOverwriteBody", { name: chain.name, agent: agentName })
            : t("mcp.driftBody", { name: chain.name, agent: agentName })}
        </p>

        {/* Scrollable body: two verbatim file panes, never a synthesized diff. */}
        <div className="grid min-h-0 flex-1 grid-cols-1 gap-3 overflow-y-auto md:grid-cols-2">
          <div className="flex min-h-0 flex-col">
            <div className="mb-1 shrink-0 text-[11px] font-semibold uppercase tracking-wide text-faint">
              {t("mcp.driftCurrent")}
            </div>
            <pre className="min-h-[120px] shrink-0 overflow-auto whitespace-pre-wrap break-all rounded-lg border border-border-subtle bg-background p-2 font-mono text-[11.5px] leading-4 text-muted">
              {head.current_text}
            </pre>
          </div>
          <div className="flex min-h-0 flex-col">
            <div className="mb-1 flex shrink-0 items-center gap-1.5">
              <span className="text-[11px] font-semibold uppercase tracking-wide text-emerald-600/80 dark:text-emerald-400/80">
                {t("mcp.driftPlanned")}
              </span>
              <span className="rounded-full bg-emerald-500/10 px-1.5 text-[10px] font-medium text-emerald-600 dark:text-emerald-400">
                {t("mcp.driftWillWrite")}
              </span>
            </div>
            <pre
              className={cn(
                "min-h-[120px] shrink-0 overflow-auto whitespace-pre-wrap break-all rounded-lg border border-border-subtle bg-background p-2 font-mono text-[11.5px] leading-4 text-secondary",
              )}
            >
              {head.planned_text}
            </pre>
          </div>
        </div>

        {(applied.length > 0 || queue.length > 1) && (
          <div className="mt-3 flex shrink-0 flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted">
            {applied.map((agent) => (
              <span
                key={agent}
                className="inline-flex items-center gap-1 text-emerald-600 dark:text-emerald-400"
              >
                <Check className="h-3 w-3" strokeWidth={3} />
                {chain.agentLabel?.(agent) ?? agent}
              </span>
            ))}
            {queue.length > 1 && (
              <span>
                {t("mcp.driftQueued", {
                  agents: queue
                    .slice(1)
                    .map((d) => chain.agentLabel?.(d.agent_key) ?? d.agent_key)
                    .join(", "),
                })}
              </span>
            )}
          </div>
        )}

        <div className="mt-4 flex shrink-0 justify-end gap-2">
          <button
            onClick={cancel}
            disabled={busy}
            className="rounded-lg px-3 py-1.5 text-[13px] font-medium text-tertiary outline-none transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50"
          >
            {t("mcp.driftCancel")}
          </button>
          <button
            onClick={() => void approve()}
            disabled={busy}
            className="inline-flex items-center gap-2 rounded-lg border border-red-500/50 bg-red-600/90 px-3 py-1.5 text-[13px] font-medium text-white outline-none transition-colors hover:bg-red-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            {t("mcp.driftConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
