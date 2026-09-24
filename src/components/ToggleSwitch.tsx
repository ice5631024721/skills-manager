import { Loader2 } from "lucide-react";
import { cn } from "../utils";

interface Props {
  checked: boolean;
  onChange: () => void;
  disabled?: boolean;
  /** Shows a spinner in the knob while the backing operation is in flight. */
  loading?: boolean;
  title?: string;
  className?: string;
  /**
   * Mixed state for group switches (some children on, some off): knob sits
   * centred on the accent track. Still clickable either way.
   */
  indeterminate?: boolean;
}

/** 34x20 pill switch — the canonical on/off control (see UI spec in CLAUDE.md). */
export function ToggleSwitch({
  checked,
  onChange,
  disabled,
  loading,
  title,
  className,
  indeterminate,
}: Props) {
  const visualOn = checked || indeterminate;
  return (
    <button
      type="button"
      role="switch"
      aria-checked={indeterminate ? "mixed" : checked}
      aria-label={title}
      aria-busy={loading || undefined}
      title={title}
      disabled={disabled || loading}
      onClick={(e) => {
        e.stopPropagation();
        onChange();
      }}
      className={cn(
        "relative h-5 w-[34px] shrink-0 rounded-full outline-none transition-colors",
        "focus-visible:ring-2 focus-visible:ring-accent",
        visualOn ? "bg-accent-light" : "bg-surface-active",
        loading
          ? "cursor-wait opacity-70"
          : disabled
            ? "cursor-not-allowed opacity-40"
            : "cursor-pointer",
        className
      )}
    >
      <span
        className={cn(
          "absolute top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-white shadow-[0_1px_2px_rgba(0,0,0,0.25)] transition-all",
          indeterminate ? "left-2" : checked ? "left-[16px]" : "left-0.5",
          indeterminate && "h-3.5 w-3.5 translate-y-0.5"
        )}
      >
        {loading && <Loader2 className="h-2.5 w-2.5 animate-spin text-muted" />}
      </span>
    </button>
  );
}
