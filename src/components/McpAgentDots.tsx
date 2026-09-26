import { Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { McpAgentStatus, McpBindingDto } from "../lib/tauri";
import { cn } from "../utils";
import { AgentIcon } from "./AgentIcon";
import { hasAgentIcon, shortLabel } from "../lib/agentIcons";

type DotState = "synced" | "drift" | "available" | "orphan";

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
  size?: "sm" | "md";
  className?: string;
  /**
   * When provided, each agent dot becomes a button: clicking syncs/unsyncs
   * the definition to that agent. The handler receives the next desired
   * state (entry presence, not the agent's native enabled field — ADR-0005
   * clause 9).
   */
  onToggle?: (agentKey: string, nextDesired: boolean) => void;
  /** Agent key currently performing a sync/unsync write; shows a loader on that dot. */
  pendingKey?: string | null;
}

export function McpAgentDots({
  agents,
  bindings,
  size = "md",
  className,
  onToggle,
  pendingKey,
}: Props) {
  const { t } = useTranslation();
  const installed = agents.filter((agent) => agent.installed);
  const installedKeys = new Set(installed.map((agent) => agent.agent_key));
  const bindingFor = (agentKey: string) =>
    bindings.find((binding) => binding.agent_key === agentKey);

  const dots: Dot[] = installed.map((agent) => {
    const binding = bindingFor(agent.agent_key);
    const state: DotState = !binding
      ? "available"
      : binding.drift
        ? "drift"
        : "synced";
    return {
      key: agent.agent_key,
      displayName: agent.display_name,
      state,
      driftReason: binding?.drift_reason ?? null,
    };
  });

  // Bindings on agents the scan no longer reports as installed.
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

  const dim =
    size === "sm" ? "h-[16px] w-[16px] text-[8px]" : "h-[18px] w-[18px] text-[9px]";

  const iconStateClass: Record<DotState, string> = {
    synced: "bg-surface",
    drift: "ring-1 ring-inset ring-amber-500/60 bg-surface",
    available: "bg-surface opacity-45",
    orphan: "ring-1 ring-inset ring-amber-500/60 bg-surface opacity-70",
  };

  const textStateClass: Record<DotState, string> = {
    synced: "border-transparent bg-[var(--color-text-primary)] text-[var(--color-bg)]",
    drift: "border border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400",
    available: "border border-border-subtle bg-surface-hover text-faint",
    orphan:
      "border border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400 opacity-70",
  };

  const stateTitle: Record<DotState, string> = {
    synced: ` · ${t("mcp.dot.synced")}`,
    drift: ` · ${t("mcp.dot.drift")}`,
    available: "",
    orphan: ` · ${t("mcp.dot.agentUnavailable")}`,
  };

  // A dot with a binding is a click-to-unsync; a free one is click-to-sync.
  const hasBinding: Record<DotState, boolean> = {
    synced: true,
    drift: true,
    available: false,
    orphan: true,
  };

  return (
    <div className={cn("flex items-center gap-[2px]", className)}>
      {dots.map((dot) => {
        const useIcon = hasAgentIcon(dot.key);
        const isPending = pendingKey === dot.key;
        const interactive = !!onToggle && !isPending;
        const clickHint = onToggle
          ? ` · ${t(hasBinding[dot.state] ? "mcp.unsyncFrom" : "mcp.syncTo")}`
          : "";
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

        if (onToggle) {
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
                onToggle(dot.key, !hasBinding[dot.state]);
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
