import type { ReactNode } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { cn } from "../utils";

interface Props {
  icon: ReactNode;
  title: string;
  description?: string | null;
  /** Number of skills currently shown inside the section. */
  count: number;
  collapsed: boolean;
  onToggle: () => void;
  /** Group-level switch (e.g. enable/disable the whole group for the preset). */
  toggle?: ReactNode;
  /** Aggregate status pills (updates available, upstream deleted…). */
  badges?: ReactNode;
  /** Group-level agent sync dots (install/uninstall the whole group). */
  syncDots?: ReactNode;
  /** Right-side overflow menu node (already built, e.g. a CardActionMenu). */
  menu?: ReactNode;
  children: ReactNode;
}

/**
 * Collapsible group section for the MySkills management view (Skill Source /
 * Collection perspectives). Renders as one bordered card: a prominent header
 * row (collapse toggle, icon chip, title, count, description, group switch,
 * overflow menu) with the skill grid/list inside. The header row toggles
 * collapse; interactive slots sit outside that button so they never nest.
 */
export function SkillGroupSection({
  icon,
  title,
  description,
  count,
  collapsed,
  onToggle,
  toggle,
  badges,
  syncDots,
  menu,
  children,
}: Props) {
  return (
    <section className="rounded-xl border border-border bg-background">
      <div
        className={cn(
          "flex items-center gap-2 px-3 py-2.5",
          !collapsed && "border-b border-border-subtle"
        )}
      >
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={!collapsed}
          title={title}
          className="flex min-w-0 flex-1 items-center gap-2.5 rounded-lg px-1 py-0.5 text-left outline-none transition-colors hover:bg-surface-hover focus-visible:ring-2 focus-visible:ring-border"
        >
          {collapsed ? (
            <ChevronRight className="h-4 w-4 shrink-0 text-muted" />
          ) : (
            <ChevronDown className="h-4 w-4 shrink-0 text-muted" />
          )}
          <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg border border-border-subtle bg-surface-hover text-secondary">
            {icon}
          </span>
          <span className="truncate text-[14px] font-semibold text-primary" title={title}>
            {title}
          </span>
          <span className="shrink-0 rounded-full border border-border bg-surface-hover px-2 text-[11px] font-semibold leading-5 text-tertiary tabular-nums">
            {count}
          </span>
          {description ? (
            <span
              className={cn("hidden min-w-0 truncate text-[12px] text-muted lg:block")}
              title={description}
            >
              {description}
            </span>
          ) : null}
        </button>
        {badges}
        {syncDots}
        {toggle}
        {menu}
      </div>
      {!collapsed && <div className="px-3 pb-3 pt-3">{children}</div>}
    </section>
  );
}
