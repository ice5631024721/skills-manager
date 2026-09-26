import { useEffect, useMemo, useState } from "react";
import { Eye, EyeOff, Loader2, Plus, Trash2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { cn } from "../utils";
import { getErrorMessage } from "../lib/error";
import * as api from "../lib/tauri";
import type { McpEntryDef, McpServerDto, McpSource } from "../lib/tauri";
import type { McpDriftChain } from "./McpDriftDialog";

// ── client-side source inference ──
// Mirror of `mcp_upstream::infer_source` (src-tauri/src/core/mcp_upstream.rs).
// Keep in sync with the Rust rules; the backend stays authoritative, this is
// only the "auto" prefill so the dialog can preview what will be stored.

const TRANSPORTS = ["stdio", "http", "streamable-http"] as const;

type SourceKind = "auto" | McpSource["kind"];

/** Lowercase basename of the command with any `.exe` suffix removed — so
 *  bare `npx`, `/…/fnm/…/bin/npx` and `node.exe` classify the same. */
function commandTool(command: string): string {
  const trimmed = command.trim();
  const last = trimmed.split(/[/\\]/).pop() ?? trimmed;
  return (last.endsWith(".exe") ? last.slice(0, -4) : last).toLowerCase();
}

/** Flags whose following argument is their value, not a positional. */
function isValueConsumingFlag(arg: string): boolean {
  return arg === "-p" || arg === "--package" || arg === "--from" || arg === "--with";
}

/** `pkg@1.2.3` → `pkg`; `@scope/name` keeps its leading `@`. */
function stripVersionSpec(arg: string): string {
  const idx = arg.lastIndexOf("@");
  return idx > 0 ? arg.slice(0, idx) : arg;
}

/** First positional argument, skipping boolean flags and flag values. */
function firstPositional(args: string[]): string | null {
  let idx = 0;
  while (idx < args.length) {
    const arg = args[idx];
    if (isValueConsumingFlag(arg)) {
      idx += 2;
      continue;
    }
    if (arg.startsWith("-")) {
      idx += 1;
      continue;
    }
    return stripVersionSpec(arg);
  }
  return null;
}

/** Value of the first occurrence of `flag` (`uvx --from pkg entry` → pkg). */
function flagValue(args: string[], flag: string): string | null {
  const idx = args.indexOf(flag);
  return idx >= 0 && idx + 1 < args.length ? stripVersionSpec(args[idx + 1]) : null;
}

/** Package id from `…/node_modules/@scope/name/bin.mjs` (last node_modules
 *  occurrence wins, so nested installs resolve to the innermost package).
 *  Mirrors Rust `package_from_node_modules_path` EXACTLY: the forward-slash
 *  marker wins when present; the backslash form is only a fallback — taking
 *  the later of the two (as an earlier draft did) diverges on mixed-separator
 *  paths and would prefill a different package than the backend infers. */
function packageFromNodeModulesPath(path: string): string | null {
  const slash = path.lastIndexOf("node_modules/");
  const back = path.lastIndexOf("node_modules\\");
  const marker = slash >= 0 ? slash : back;
  if (marker < 0) return null;
  const rest = path.slice(marker + "node_modules".length + 1);
  const segments = rest.split(/[/\\]/).filter((s) => s.length > 0);
  const [first, second] = segments;
  if (first && second && first.startsWith("@")) return `${first}/${second}`;
  if (first && !first.startsWith("@")) return first;
  return null;
}

function inferMcpSource(command: string | null | undefined, args: string[]): McpSource {
  if (!command) return { kind: "none" };
  const tool = commandTool(command);
  if (tool === "npx") {
    const pkg = firstPositional(args);
    return pkg ? { kind: "npx", package: pkg } : { kind: "none" };
  }
  if (tool === "uvx") {
    // `uvx --from <pkg> <entrypoint>`: the package is the --from value, not
    // the entrypoint. Plain `uvx <pkg>` takes the first positional.
    const pkg = flagValue(args, "--from") ?? firstPositional(args);
    return pkg ? { kind: "pypi_uvx", package: pkg } : { kind: "none" };
  }
  if (tool === "node" || tool === "nodejs" || tool === "bun") {
    // Only the absolute-install shape counts: the entry script must live
    // inside node_modules/<package> (a bare `node server.js` stays none).
    const pkg = packageFromNodeModulesPath(args[0] ?? "");
    return pkg ? { kind: "npm_global", package: pkg } : { kind: "none" };
  }
  // Rust parity: absolute paths like `…/current/bin/node` end with the tool
  // name even when their basename lookup was already checked above; re-test
  // the whole (trimmed, lowered, .exe-stripped) command against `node`.
  let lowered = command.trim().toLowerCase();
  if (lowered.endsWith(".exe")) lowered = lowered.slice(0, -4);
  if (lowered.endsWith("node")) {
    const pkg = packageFromNodeModulesPath(args[0] ?? "");
    if (pkg) return { kind: "npm_global", package: pkg };
  }
  return { kind: "none" };
}

// ── JSON paste normalization ──

interface PastedEntry {
  name: string;
  def: McpEntryDef;
}

interface PasteParse {
  entries: PastedEntry[];
  /** Names whose object carried neither command nor url (reported, skipped). */
  skipped: string[];
  /** Fatal whole-document problem: bad JSON or an unrecognized shape. */
  error: string | null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function stringList(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((v): v is string => typeof v === "string") : [];
}

function envMap(...candidates: unknown[]): Record<string, string> {
  for (const candidate of candidates) {
    if (!isRecord(candidate)) continue;
    const out: Record<string, string> = {};
    for (const [key, value] of Object.entries(candidate)) {
      if (value !== null && value !== undefined) out[key] = String(value);
    }
    return out;
  }
  return {};
}

/** One pasted server object → McpEntryDef. Accepts the Claude spelling
 *  (command string + args[], env{}), the opencode spelling (type "local"
 *  with a command ARRAY and environment{}), and remote spellings —
 *  opencode "remote" can't distinguish http from streamable-http, so remote
 *  entries normalize to "http" unless the object says otherwise. */
function normalizePastedEntry(name: string, value: unknown): PastedEntry | null {
  if (!isRecord(value)) return null;
  const type = typeof value.type === "string" ? value.type.toLowerCase() : null;
  const url = typeof value.url === "string" && value.url ? value.url : null;
  const env = envMap(value.env, value.environment);
  if (Array.isArray(value.command) && stringList(value.command).length > 0) {
    const parts = stringList(value.command);
    return { name, def: { name, transport: "stdio", command: parts[0], args: parts.slice(1), url, env } };
  }
  if (typeof value.command === "string" && value.command.trim()) {
    return {
      name,
      def: { name, transport: "stdio", command: value.command.trim(), args: stringList(value.args), url, env },
    };
  }
  if (url) {
    const transport = type === "streamable-http" ? "streamable-http" : "http";
    return { name, def: { name, transport, command: null, args: [], url, env } };
  }
  return null;
}

/** Detects the wrapper and normalizes every server object found:
 *  (a) Claude style {mcpServers:{name:{…}}}, (b) opencode {mcp:{name:{…}}},
 *  (c) a bare {name:{…}} map. */
function parsePastedServers(text: string): PasteParse {
  const empty: PasteParse = { entries: [], skipped: [], error: null };
  const trimmed = text.trim();
  if (!trimmed) return empty;
  let root: unknown;
  try {
    root = JSON.parse(trimmed);
  } catch {
    return { ...empty, error: "json" };
  }
  if (!isRecord(root)) return { ...empty, error: "shape" };
  let map: Record<string, unknown> | null = null;
  if (isRecord(root.mcpServers)) map = root.mcpServers;
  else if (isRecord(root.mcp)) map = root.mcp;
  else if (Object.values(root).some(isRecord)) map = root;
  if (!map) return { ...empty, error: "shape" };
  const entries: PastedEntry[] = [];
  const skipped: string[] = [];
  for (const [name, value] of Object.entries(map)) {
    if (!isRecord(value)) continue;
    const entry = normalizePastedEntry(name.trim(), value);
    if (entry) entries.push(entry);
    else skipped.push(name);
  }
  if (entries.length === 0) return { entries, skipped, error: "shape" };
  return { entries, skipped, error: null };
}

// ── dialog ──

interface Props {
  open: boolean;
  /** Set → edit this managed definition; unset → add a new one. */
  server?: McpServerDto | null;
  onClose: () => void;
  /** Re-pull the library after any successful write. */
  onChanged: () => void;
  /** Hand a drift queue to the drift-confirmation chain (Task 12). */
  runDriftChain: (chain: McpDriftChain) => void;
  /** Agent key → display name for drift progress. */
  agentLabel: (agentKey: string) => string;
}

interface EnvRow {
  key: string;
  value: string;
  revealed: boolean;
}

const inputClass =
  "w-full rounded-lg border border-border-subtle bg-background px-3 py-2 text-[13px] text-secondary outline-none transition-all placeholder:text-faint focus:border-border";

/** Form or JSON-paste dialog over one definition (ADR-0006 §3 identity: the
 *  name is editable — rename is a supported backend flow). */
export function McpAddDialog({ open, server, onClose, onChanged, runDriftChain, agentLabel }: Props) {
  const { t } = useTranslation();
  const editing = !!server;
  const [mode, setMode] = useState<"form" | "paste">("form");
  const [name, setName] = useState("");
  const [transport, setTransport] = useState<string>("stdio");
  /** Whole stdio command line: the first token is the command, the rest
   *  space-split into args (mirrors the backend's `[command, …args].join(" ")`
   *  display that the card already shows). */
  const [commandLine, setCommandLine] = useState("");
  const [url, setUrl] = useState("");
  const [envRows, setEnvRows] = useState<EnvRow[]>([]);
  const [sourceKind, setSourceKind] = useState<SourceKind>("auto");
  const [sourceValue, setSourceValue] = useState("");
  const [pasteText, setPasteText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // (Re)initialize every time the dialog opens; the paste mode is add-only.
  useEffect(() => {
    if (!open) return;
    setError(null);
    setBusy(false);
    setPasteText("");
    setMode("form");
    if (server) {
      setName(server.name);
      setTransport(TRANSPORTS.includes(server.transport as (typeof TRANSPORTS)[number]) ? server.transport : "stdio");
      setCommandLine(server.command ? [server.command, ...server.args].join(" ") : "");
      setUrl(server.url ?? "");
      setEnvRows(
        Object.entries(server.env).map(([key, value]) => ({ key, value, revealed: false })),
      );
      setSourceKind(server.source.kind);
      setSourceValue(
        server.source.kind === "git"
          ? server.source.repo_url
          : "package" in server.source
            ? server.source.package
            : "",
      );
    } else {
      setName("");
      setTransport("stdio");
      setCommandLine("");
      setUrl("");
      setEnvRows([]);
      setSourceKind("auto");
      setSourceValue("");
    }
  }, [open, server]);

  // Escape closes, except while a write is in flight.
  useEffect(() => {
    if (!open || busy) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, busy, onClose]);

  const pasted = useMemo(
    () => (mode === "paste" ? parsePastedServers(pasteText) : null),
    [mode, pasteText],
  );

  const commandParts = useMemo(
    () => (commandLine.trim() ? commandLine.trim().split(/\s+/) : []),
    [commandLine],
  );
  const inferred = useMemo(
    () => (transport === "stdio" ? inferMcpSource(commandParts[0] ?? null, commandParts.slice(1)) : { kind: "none" } as McpSource),
    [transport, commandParts],
  );

  if (!open) return null;

  const setEnvRow = (index: number, patch: Partial<EnvRow>) =>
    setEnvRows((prev) => prev.map((row, i) => (i === index ? { ...row, ...patch } : row)));

  /** Collect the form into an entry, or return the validation message. */
  const buildEntry = (): McpEntryDef | string => {
    const trimmedName = name.trim();
    if (!trimmedName) return t("mcp.validationNameRequired");
    if (trimmedName.includes("/") || trimmedName.includes("\\"))
      return t("mcp.validationNameSlash");
    if (transport === "stdio" && commandParts.length === 0) return t("mcp.validationCommandRequired");
    if (transport !== "stdio" && !url.trim()) return t("mcp.validationUrlRequired");
    const rows = envRows.filter((row) => row.key.trim() || row.value);
    if (rows.some((row) => !row.key.trim())) return t("mcp.validationEnvKeyRequired");
    if (sourceKind !== "auto" && sourceKind !== "none" && !sourceValue.trim())
      return t("mcp.validationSourceValueRequired");
    const env: Record<string, string> = {};
    for (const row of rows) env[row.key.trim()] = row.value;
    if (transport === "stdio") {
      return {
        name: trimmedName,
        transport,
        command: commandParts[0],
        args: commandParts.slice(1),
        url: null,
        env,
      };
    }
    return { name: trimmedName, transport, command: null, args: [], url: url.trim(), env };
  };

  const buildSource = (entry: McpEntryDef): McpSource => {
    switch (sourceKind) {
      case "auto":
        return inferMcpSource(entry.command, entry.args ?? []);
      case "none":
        return { kind: "none" };
      case "npm_global":
      case "npx":
      case "pypi_uvx":
        return { kind: sourceKind, package: sourceValue.trim() };
      case "git":
        // clone_path is re-derived server-side (ensure_git_clone) on add/edit.
        return { kind: "git", repo_url: sourceValue.trim(), clone_path: "" };
    }
  };

  const submitForm = async () => {
    const entry = buildEntry();
    if (typeof entry === "string") {
      setError(entry);
      return;
    }
    const source = buildSource(entry);
    setBusy(true);
    setError(null);
    try {
      if (server) {
        const outcome = await api.editMcpServer(server.id, entry, source, null);
        onChanged();
        if (outcome.pending_drift.length > 0) {
          // Hand the drifted agents to the drift chain; the record itself is
          // already saved, each retry replays the edit with one fresh token.
          onClose();
          runDriftChain({
            name: entry.name,
            queue: outcome.pending_drift,
            applied: outcome.applied,
            retry: (token) => api.editMcpServer(server.id, entry, source, token),
            agentLabel,
            onDone: ({ cancelled }) => {
              if (cancelled === null) toast.success(t("mcp.editSavedToast", { name: entry.name }));
              else toast.info(t("mcp.driftStopped", { name: entry.name }));
              onChanged();
            },
          });
          return;
        }
        toast.success(t("mcp.editSavedToast", { name: entry.name }));
      } else {
        // The dialog stays on the definition; agent sync lives on the dots.
        await api.addMcpServer(entry, source, []);
        toast.success(t("mcp.addSavedToast", { name: entry.name }));
        onChanged();
      }
      onClose();
    } catch (err) {
      setError(null);
      toast.error(getErrorMessage(err, t("common.error")));
      // A failed edit can still have committed the record (the backend
      // updates the row before the per-agent write loop) — the card grid
      // must re-pull, same report-then-refresh pattern as runDelete.
      onChanged();
    } finally {
      setBusy(false);
    }
  };

  const submitPaste = async () => {
    if (!pasted || pasted.entries.length === 0) {
      setError(t("mcp.validationPasteEmpty"));
      return;
    }
    setBusy(true);
    setError(null);
    const added: string[] = [];
    const failed: string[] = [];
    for (const item of pasted.entries) {
      // Continue on failure: a name collision rejects only its own entry.
      try {
        await api.addMcpServer(
          item.def,
          inferMcpSource(item.def.command, item.def.args ?? []),
          [],
        );
        added.push(item.name);
      } catch (err) {
        failed.push(`${item.name}: ${getErrorMessage(err, t("common.error"))}`);
      }
    }
    onChanged();
    setBusy(false);
    if (failed.length === 0) {
      toast.success(t("mcp.pasteAddedToast", { count: added.length }), {
        description: added.join(", "),
      });
      onClose();
    } else if (added.length > 0) {
      toast.warning(t("mcp.pastePartialToast", { added: added.length, failed: failed.length }), {
        description: failed.join("\n"),
        duration: 10000,
      });
    } else {
      toast.error(t("mcp.pasteFailedToast", { count: failed.length }), {
        description: failed.join("\n"),
        duration: 10000,
      });
    }
  };

  const labelClass = "mb-1 block text-[12px] font-medium text-tertiary";
  const iconButtonClass =
    "rounded-md p-1.5 text-muted transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50";

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div
        className="absolute inset-0 bg-black/70 backdrop-blur-sm"
        onClick={busy ? undefined : onClose}
      />
      {/* Capped shell (same calc as ConfirmDialog) with a scrolling body so
          the footer buttons stay reachable at every text scale. */}
      <div className="relative flex max-h-[calc(85vh/var(--app-scale))] w-full max-w-lg flex-col rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-4 flex shrink-0 items-center justify-between">
          <h2 className="text-[13px] font-semibold text-primary">
            {t(editing ? "mcp.editServer" : "mcp.addServer")}
          </h2>
          <button
            onClick={onClose}
            disabled={busy}
            className="rounded p-1 text-muted outline-none transition-colors hover:text-secondary disabled:opacity-50"
            aria-label={t("common.cancel")}
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        {!editing && (
          <div className="mb-4 flex shrink-0 gap-1 rounded-lg border border-border-subtle bg-background p-0.5">
            {(["form", "paste"] as const).map((key) => (
              <button
                key={key}
                type="button"
                onClick={() => setMode(key)}
                className={cn(
                  "flex-1 rounded-md py-1.5 text-[13px] font-medium outline-none transition-all",
                  mode === key ? "bg-surface text-primary shadow-sm" : "text-muted hover:text-secondary",
                )}
              >
                {t(key === "form" ? "mcp.addModeForm" : "mcp.addModePaste")}
              </button>
            ))}
          </div>
        )}

        <div className="min-h-0 flex-1 overflow-y-auto">
          {mode === "form" ? (
            <div className="space-y-3">
              <div>
                <label className={labelClass} htmlFor="mcp-field-name">
                  {t("mcp.fieldName")}
                </label>
                <input
                  id="mcp-field-name"
                  type="text"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="my-server"
                  className={cn(inputClass, "font-mono")}
                />
              </div>

              <div>
                <label className={labelClass} htmlFor="mcp-field-transport">
                  {t("mcp.fieldTransport")}
                </label>
                <select
                  id="mcp-field-transport"
                  value={transport}
                  onChange={(e) => setTransport(e.target.value)}
                  className={inputClass}
                >
                  {TRANSPORTS.map((option) => (
                    <option key={option} value={option}>
                      {option}
                    </option>
                  ))}
                </select>
              </div>

              {transport === "stdio" ? (
                <div>
                  <label className={labelClass} htmlFor="mcp-field-command">
                    {t("mcp.fieldCommand")}
                  </label>
                  <input
                    id="mcp-field-command"
                    type="text"
                    value={commandLine}
                    onChange={(e) => setCommandLine(e.target.value)}
                    placeholder="npx -y @scope/package"
                    className={cn(inputClass, "font-mono")}
                  />
                </div>
              ) : (
                <div>
                  <label className={labelClass} htmlFor="mcp-field-url">
                    {t("mcp.fieldUrl")}
                  </label>
                  <input
                    id="mcp-field-url"
                    type="text"
                    value={url}
                    onChange={(e) => setUrl(e.target.value)}
                    placeholder="http://127.0.0.1:7999/mcp"
                    className={cn(inputClass, "font-mono")}
                  />
                </div>
              )}

              <div>
                <div className="mb-1 flex items-center justify-between">
                  <span className={cn(labelClass, "mb-0")}>{t("mcp.envHeading")}</span>
                  <button
                    type="button"
                    onClick={() => setEnvRows((prev) => [...prev, { key: "", value: "", revealed: false }])}
                    className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-[12px] font-medium text-muted transition-colors hover:bg-surface-hover hover:text-secondary"
                  >
                    <Plus className="h-3 w-3" />
                    {t("mcp.envAdd")}
                  </button>
                </div>
                {envRows.length === 0 ? (
                  <div className="rounded-lg border border-dashed border-border-subtle px-3 py-2 text-[12px] text-faint">
                    {t("mcp.envEmpty")}
                  </div>
                ) : (
                  <div className="space-y-2">
                    {envRows.map((row, index) => (
                      <div key={index} className="flex items-center gap-2">
                        <input
                          type="text"
                          value={row.key}
                          onChange={(e) => setEnvRow(index, { key: e.target.value })}
                          placeholder={t("mcp.envKey")}
                          className={cn(inputClass, "flex-1 font-mono")}
                        />
                        <input
                          type={row.revealed ? "text" : "password"}
                          value={row.value}
                          onChange={(e) => setEnvRow(index, { value: e.target.value })}
                          placeholder={t("mcp.envValue")}
                          className={cn(inputClass, "flex-1 font-mono")}
                        />
                        <button
                          type="button"
                          onClick={() => setEnvRow(index, { revealed: !row.revealed })}
                          className={iconButtonClass}
                          title={t("mcp.envReveal")}
                          aria-label={t("mcp.envReveal")}
                        >
                          {row.revealed ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                        </button>
                        <button
                          type="button"
                          onClick={() => setEnvRows((prev) => prev.filter((_, i) => i !== index))}
                          className={cn(iconButtonClass, "hover:text-red-500")}
                          title={t("mcp.envRemove")}
                          aria-label={t("mcp.envRemove")}
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    ))}
                  </div>
                )}
              </div>

              <div>
                <label className={labelClass} htmlFor="mcp-field-source">
                  {t("mcp.sourceHeading")}
                </label>
                <div className="flex items-center gap-2">
                  <select
                    id="mcp-field-source"
                    value={sourceKind}
                    onChange={(e) => setSourceKind(e.target.value as SourceKind)}
                    className={cn(inputClass, "w-44 shrink-0")}
                  >
                    <option value="auto">{t("mcp.sourceAuto")}</option>
                    <option value="none">{t("mcp.sourceNone")}</option>
                    <option value="npm_global">{t("mcp.sourceNpmGlobal")}</option>
                    <option value="npx">{t("mcp.sourceNpx")}</option>
                    <option value="pypi_uvx">{t("mcp.sourcePypiUvx")}</option>
                    <option value="git">{t("mcp.sourceGit")}</option>
                  </select>
                  {sourceKind !== "auto" && sourceKind !== "none" && (
                    <input
                      type="text"
                      value={sourceValue}
                      onChange={(e) => setSourceValue(e.target.value)}
                      placeholder={t(sourceKind === "git" ? "mcp.sourceRepoUrl" : "mcp.sourcePackage")}
                      className={cn(inputClass, "flex-1 font-mono")}
                    />
                  )}
                </div>
                {sourceKind === "auto" && (
                  <div className="mt-1 font-mono text-[11px] text-faint">
                    {t("mcp.sourceAutoHint")} →{" "}
                    {inferred.kind === "none"
                      ? inferred.kind
                      : `${inferred.kind}: ${inferred.kind === "git" ? inferred.repo_url : inferred.package}`}
                  </div>
                )}
              </div>
            </div>
          ) : (
            <div className="space-y-3">
              <textarea
                value={pasteText}
                onChange={(e) => setPasteText(e.target.value)}
                placeholder={t("mcp.pastePlaceholder")}
                spellCheck={false}
                className={cn(inputClass, "h-56 resize-y font-mono text-[12px] leading-5")}
              />
              {pasted && pasted.error && (
                <div className="text-[12px] text-red-500">
                  {pasted.error === "json" ? t("mcp.pasteInvalidJson") : t("mcp.pasteInvalidShape")}
                </div>
              )}
              {pasted && !pasted.error && pasted.entries.length > 0 && (
                <div>
                  <div className="mb-1 text-[12px] text-muted">
                    {t("mcp.pasteDetected", { count: pasted.entries.length })}
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {pasted.entries.map((item) => (
                      <span
                        key={item.name}
                        className="rounded-full border border-border-subtle bg-surface-hover px-2 py-0.5 font-mono text-[11px] text-secondary"
                      >
                        {item.name}
                      </span>
                    ))}
                  </div>
                  {pasted.skipped.length > 0 && (
                    <div className="mt-1 text-[11px] text-faint">
                      {t("mcp.pasteSkipped", { names: pasted.skipped.join(", ") })}
                    </div>
                  )}
                </div>
              )}
            </div>
          )}
        </div>

        <div className="mt-4 flex shrink-0 items-center justify-end gap-2">
          {error && <span className="mr-auto min-w-0 truncate text-[12px] text-red-500">{error}</span>}
          <button
            type="button"
            onClick={onClose}
            disabled={busy}
            className="rounded-lg px-3 py-1.5 text-[13px] font-medium text-tertiary outline-none transition-colors hover:bg-surface-hover hover:text-secondary disabled:opacity-50"
          >
            {t("common.cancel")}
          </button>
          <button
            type="button"
            onClick={() => void (mode === "form" ? submitForm() : submitPaste())}
            disabled={busy || (mode === "paste" && (pasted?.entries.length ?? 0) === 0)}
            className="inline-flex items-center gap-2 rounded-lg border border-accent-border bg-accent-dark px-4 py-1.5 text-[13px] font-medium text-white outline-none transition-colors hover:bg-accent disabled:cursor-not-allowed disabled:opacity-50"
          >
            {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            {t(mode === "form" && editing ? "common.save" : "common.create")}
          </button>
        </div>
      </div>
    </div>
  );
}
