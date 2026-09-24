import { Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { ManagedSkill, ToolInfo } from "../lib/tauri";
import { cn } from "../utils";
import { AgentIcon } from "./AgentIcon";
import { hasAgentIcon } from "../lib/agentIcons";
import { shortLabel } from "../lib/agentIcons";

/**
 * Group-level counterpart of `SyncDots`: one dot per agent tool, aggregated
 * over every skill in the group (库维度). "all" installs/uninstalls the whole
 * group for that agent; "partial" (some skills synced) reads as a ringed dot
 * and clicking it installs the rest.
 */
type GroupDotState = "all" | "partial" | "none";

interface Dot {
  key: string;
  displayName: string;
  state: GroupDotState;
  synced: number;
  total: number;
}

interface Props {
  skills: ManagedSkill[];
  tools: ToolInfo[];
  limit?: number;
  size?: "sm" | "md";
  className?: string;
  /** Click a dot to install/uninstall the whole group for that agent. */
  onToggle?: (toolKey: string, enabled: boolean) => void;
  /** Tool key currently performing a group sync/unsync; shows a loader. */
  pendingKey?: string | null;
}

export function GroupSyncDots({
  skills,
  tools,
  limit,
  size = "md",
  className,
  onToggle,
  pendingKey,
}: Props) {
  const { t } = useTranslation();
  const activeTools = tools.filter((tool) => tool.installed && tool.enabled);
  const total = skills.length;

  const dots: Dot[] = activeTools.map((tool) => {
    const synced = skills.filter((skill) =>
      skill.targets.some((target) => target.tool === tool.key)
    ).length;
    const state: GroupDotState =
      total > 0 && synced === total ? "all" : synced > 0 ? "partial" : "none";
    return { key: tool.key, displayName: tool.display_name, state, synced, total };
  });

  const visible = typeof limit === "number" ? dots.slice(0, limit) : dots;
  const hiddenCount = dots.length - visible.length;
  if (visible.length === 0) return null;

  const dim =
    size === "sm" ? "h-[16px] w-[16px] text-[8px]" : "h-[18px] w-[18px] text-[9px]";

  const iconStateClass: Record<GroupDotState, string> = {
    all: "bg-surface",
    partial: "bg-surface ring-1 ring-inset ring-accent/60",
    none: "bg-surface opacity-45",
  };

  const textStateClass: Record<GroupDotState, string> = {
    all: "border-transparent bg-[var(--color-text-primary)] text-[var(--color-bg)]",
    partial: "border border-accent/50 bg-surface-hover text-secondary",
    none: "border border-border-subtle bg-surface-hover text-faint",
  };

  return (
    <div className={cn("flex items-center gap-[2px]", className)}>
      {visible.map((dot) => {
        const useIcon = hasAgentIcon(dot.key);
        const isPending = pendingKey === dot.key;
        const interactive = !!onToggle && !isPending;
        const stateLabel =
          dot.state === "all"
            ? t("mySkills.group.syncAll", { total: dot.total })
            : dot.state === "partial"
              ? t("mySkills.group.syncPartial", { synced: dot.synced, total: dot.total })
              : t("mySkills.group.syncNone", { total: dot.total });
        const hint = onToggle
          ? ` · ${dot.state === "all" ? t("mySkills.targetClickUninstall") : t("mySkills.targetClickInstall")}`
          : "";
        const title = `${dot.displayName} · ${stateLabel}${hint}`;
        const baseClass = cn(
          "inline-flex select-none items-center justify-center overflow-hidden rounded-[4px] transition-colors",
          dim,
          useIcon
            ? iconStateClass[dot.state]
            : cn(
                "border font-mono font-semibold tracking-tight",
                textStateClass[dot.state]
              ),
          interactive &&
            "cursor-pointer hover:ring-1 hover:ring-accent/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent",
          isPending && "opacity-70"
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
              disabled={isPending}
              onClick={(e) => {
                e.stopPropagation();
                onToggle(dot.key, dot.state !== "all");
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
      {hiddenCount > 0 && (
        <span
          className="ml-0.5 inline-flex items-center text-[11px] font-medium text-muted"
          title={dots
            .slice(visible.length)
            .map((d) => d.displayName)
            .join(", ")}
        >
          +{hiddenCount}
        </span>
      )}
    </div>
  );
}