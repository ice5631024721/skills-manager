import { useEffect, useState } from "react";
import { X } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { SkillSource } from "../lib/tauri";

interface Props {
  open: boolean;
  source: SkillSource | null;
  /** Rejects on failure (toast shown by the host) so the dialog stays open. */
  onSubmit: (description: string | null) => Promise<void>;
  onClose: () => void;
}

/**
 * Edit a Skill Source's description (MySkills source view). An empty input clears
 * the user-written description, handing control back to the auto/GitHub one.
 */
export function SourceDescriptionDialog({ open, source, onSubmit, onClose }: Props) {
  const { t } = useTranslation();
  const [description, setDescription] = useState("");
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!open) return;
    setDescription(source?.description ?? "");
  }, [open, source]);

  // Escape closes the dialog, except while the submit is running.
  useEffect(() => {
    if (!open || loading) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, loading, onClose]);

  if (!open || !source) return null;

  const handleSubmit = async () => {
    if (loading) return;
    setLoading(true);
    try {
      const trimmed = description.trim();
      await onSubmit(trimmed === "" ? null : trimmed);
      onClose();
    } catch {
      // Host reported the failure; keep the dialog open for another try.
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={onClose} />
      <div className="relative bg-surface border border-border rounded-xl w-full max-w-[420px] p-5 shadow-2xl">
        <div className="flex items-center justify-between mb-4">
          <h2 className="text-[13px] font-semibold text-primary">
            {t("mySkills.sourceGroup.descriptionTitle")}
          </h2>
          <button
            onClick={onClose}
            className="text-muted hover:text-secondary p-1 rounded transition-colors outline-none"
          >
            <X className="w-4 h-4" />
          </button>
        </div>

        <p className="mb-3 truncate text-[12px] text-muted" title={source.display_url}>
          {source.display_url}
        </p>
        <input
          type="text"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleSubmit()}
          placeholder={t("mySkills.sourceGroup.descriptionPlaceholder")}
          className="w-full bg-background border border-border-subtle rounded-lg px-3 py-2 text-[13px] text-secondary focus:outline-none focus:border-border transition-all placeholder-faint"
          autoFocus
        />
        <p className="mt-2 text-[12px] text-faint">
          {t("mySkills.sourceGroup.descriptionHint")}
        </p>

        <div className="mt-4 flex justify-end gap-2">
          <button
            onClick={onClose}
            className="px-3 py-1.5 rounded-lg text-[13px] font-medium text-tertiary hover:text-secondary hover:bg-surface-hover transition-colors outline-none"
          >
            {t("common.cancel")}
          </button>
          <button
            onClick={handleSubmit}
            disabled={loading}
            className="px-3 py-1.5 rounded-lg bg-accent-dark hover:bg-accent text-white text-[13px] font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed border border-accent-border outline-none"
          >
            {loading ? t("common.loading") : t("common.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
