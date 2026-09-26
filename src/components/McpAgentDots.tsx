import { Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { McpAgentStatus, McpBindingDto } from "../lib/tauri";
import { cn } from "../utils";
import { AgentIcon } from "./AgentIcon";
import { hasAgentIcon, shortLabel } from "../lib/agentIcons";

/**
 * synced  — binding present, no drift. Click unsyncs (through the drift chain
 *           if the write comes back pending).
 * drift   — binding present, live copy differs from the ledger (amber ring).
 * variant — binding present and matching the ledger, but the agent runs its
 *           own command kept from takeover (violet ring). Not drift: nobody
 *           changed anything. Click aligns the agent to the definition.
 * foreign — the inventory has an entry for this name in this agent but there
 *           is NO binding (dashed grey-blue ring). On a managed card the
 *           click proposes an overwrite (sync → backend returns pending_drift
 *           → the drift chain shows current-vs-planned); on a foreign card
 *           the click takes over just that agent.
 * absent  — managed cards only: neither binding nor inventory entry. Dim.
 * orphan  — binding on an agent the scan no longer reports as installed.
 */
type DotState = "synced" | "drift" | "variant" | "foreign" | "absent" | "orphan";

interface Dot {
  key: string;
  displayName: string;
  state: DotState;
  /** Why a drifted binding's live entry differs from the ledger. */
  driftReason: string | null;
}

interface Props {
  /** Supported agents from the inventory scan; only installed ones get a dot. */
  agents: McpAgentStatus[];
  /** This server's bindings. A binding whose agent vanished still shows a dot
   *  (mirrors SyncDots' `includeOrphan`): sync state must not silently
   *  disappear while the ledger still claims a deployment. */
  bindings: McpBindingDto[];
  /** Agent keys where the inventory found an entry for this server name.
   *  Present here + no binding = the foreign state. */
  presence?: Set<string>;
  /** "managed" (default) renders every installed agent (synced/drift/foreign/
   *  absent). "foreign" renders only the agents that actually have a foreign
   *  copy — the takeover affordance that replaces the old per-agent arrows. */
  variant?: "managed" | "foreign";
  size?: "sm" | "md";
  className?: string;
  /**
   * Managed cards: each agent dot becomes a button: clicking syncs/unsyncs
   * the definition to that agent. The handler receives the next desired
   * state (entry presence, not the agent's native enabled field — ADR-0005
   * clause 9). A foreign-state click also routes here with nextDesired=true:
   * the backend answers with pending_drift, so the overwrite confirmation is
   * the drift chain's job.
   */
  onToggle?: (agentKey: string, nextDesired: boolean) => void;
  /** Foreign cards: click takes over that single agent's entry. */
  onTakeover?: (agentKey: string) => void;
  /** Agent key currently performing a sync/unsync/takeover write; shows a loader on that dot. */
  pendingKey?: string | null;
}

export function McpAgentDots({
  agents,
  bindings,
  presence,
  variant = "managed",
  size = "md",
  className,
  onToggle,
  onTakeover,
  pendingKey,
}: Props) {
  const { t } = useTranslation();
  const installed = agents.filter((agent) => agent.installed);
  const installedKeys = new Set(installed.map((agent) => agent.agent_key));
  const bindingFor = (agentKey: string) =>
    bindings.find((binding) => binding.agent_key === agentKey);

  const dots: Dot[] =
    variant === "foreign"
      ? installed
          .filter((agent) => presence?.has(agent.agent_key))
          .map((agent) => ({
            key: agent.agent_key,
            displayName: agent.display_name,
            state: "foreign" as DotState,
            driftReason: null,
          }))
      : installed.map((agent) => {
          const binding = bindingFor(agent.agent_key);
          const state: DotState = !binding
            ? presence?.has(agent.agent_key)
              ? "foreign"
              : "absent"
            : binding.drift
              ? "drift"
              : binding.variant
                ? "variant"
                : "synced";
          return {
            key: agent.agent_key,
            displayName: agent.display_name,
            state,
            driftReason: binding?.drift_reason ?? null,
          };
        });

  // Bindings on agents the scan no longer reports as installed.
  if (variant === "managed") {
    for (const binding of bindings) {
      if (installedKeys.has(binding.agent_key)) continue;
      const known = agents.find((agent) => agent.agent_key === binding.agent_key);
      dots.push({
        key: binding.agent_key,
        displayName: known?.display_name ?? binding.agent_key,
        state: "orphan",
        driftReason: binding.drift_reason ?? null,
      });
    }
  }

  const dim =
    size === "sm" ? "h-[16px] w-[16px] text-[8px]" : "h-[18px] w-[18px] text-[9px]";

  const iconStateClass: Record<DotState, string> = {
    synced: "bg-surface",
    drift: "ring-1 ring-inset ring-amber-500/60 bg-surface",
    variant: "ring-1 ring-inset ring-violet-500/60 bg-surface",
    foreign:
      "bg-surface outline outline-1 -outline-offset-1 outline-dashed outline-slate-400/80 dark:outline-slate-300/50",
    absent: "bg-surface opacity-45",
    orphan: "ring-1 ring-inset ring-amber-500/60 bg-surface opacity-70",
  };

  const textStateClass: Record<DotState, string> = {
    synced: "border-transparent bg-[var(--color-text-primary)] text-[var(--color-bg)]",
    drift: "border border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400",
    variant:
      "border border-violet-500/40 bg-violet-500/10 text-violet-600 dark:text-violet-400",
    foreign:
      "border border-dashed border-slate-400/80 bg-slate-400/10 text-slate-600 dark:border-slate-300/50 dark:text-slate-300",
    absent: "border border-border-subtle bg-surface-hover text-faint",
    orphan:
      "border border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400 opacity-70",
  };

  const stateTitle: Record<DotState, string> = {
    synced: ` · ${t("mcp.dot.synced")}`,
    drift: ` · ${t("mcp.dot.drift")}`,
    variant: ` · ${t("mcp.dot.variant")}`,
    foreign: ` · ${t("mcp.dot.foreignHere")}`,
    absent: "",
    orphan: ` · ${t("mcp.dot.agentUnavailable")}`,
  };

  // A dot with a binding is a click-to-unsync; a free one is click-to-sync
  // (managed) or click-to-take-over (foreign); the foreign dot on a managed
  // card syncs too — the backend turns that into an overwrite confirmation.
  // A variant dot clicks to ALIGN (sync the definition over the kept copy),
  // so it counts as "free" here even though a binding exists.
  const hasBinding: Record<DotState, boolean> = {
    synced: true,
    drift: true,
    variant: false,
    foreign: false,
    absent: false,
    orphan: true,
  };

  return (
    <div className={cn("flex items-center gap-[2px]", className)}>
      {dots.map((dot) => {
        const useIcon = hasAgentIcon(dot.key);
        const isPending = pendingKey === dot.key;
        const clickable =
          variant === "foreign" ? !!onTakeover : !!onToggle;
        const interactive = clickable && !isPending;
        const clickHint = !clickable
          ? ""
          : variant === "foreign"
            ? ` · ${t("mcp.takeover")}`
            : ` · ${t(hasBinding[dot.state] ? "mcp.unsyncFrom" : "mcp.syncTo")}`;
        const title = `${dot.displayName}${stateTitle[dot.state]}${
          dot.driftReason ? ` — ${dot.driftReason}` : ""
        }${clickHint}`;
        const baseClass = cn(
          "inline-flex select-none items-center justify-center overflow-hidden rounded-[4px] transition-colors",
          dim,
          useIcon
            ? iconStateClass[dot.state]
            : cn(
                "border font-mono font-semibold tracking-tight",
                textStateClass[dot.state],
              ),
          interactive &&
            "cursor-pointer hover:ring-1 hover:ring-accent/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent",
          isPending && "opacity-70",
        );
        const content = isPending ? (
          <Loader2 className="h-3 w-3 animate-spin text-muted" />
        ) : useIcon ? (
          <AgentIcon
            agentKey={dot.key}
            className="h-full w-full rounded-[4px] border-0 bg-transparent"
          />
        ) : (
          shortLabel(dot.displayName, dot.key)
        );

        if (interactive) {
          return (
            <button
              type="button"
              key={dot.key}
              title={title}
              aria-label={title}
              disabled={isPending}
              onClick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                if (variant === "foreign") onTakeover?.(dot.key);
                else onToggle?.(dot.key, !hasBinding[dot.state]);
              }}
              className={baseClass}
            >
              {content}
            </button>
          );
        }

        return (
          <span key={dot.key} title={title} className={baseClass}>
            {content}
          </span>
        );
      })}
    </div>
  );
}
