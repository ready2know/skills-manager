import { useEffect, useState } from "react";
import { AlertTriangle, Check, Download, Loader2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { cn } from "../utils";
import * as api from "../lib/tauri";
import type { ClaudeMarketplace, ClaudeMarketplaces, MarketplaceDetail } from "../lib/tauri";
import { getErrorMessage } from "../lib/error";
import { useMarketplaceBatch } from "../hooks/useMarketplaceBatch";

interface Props {
  open: boolean;
  onClose: () => void;
  /** Called once a run ends, with every marketplace it added. */
  onImported: (added: MarketplaceDetail[]) => void;
}

const importable = (item: ClaudeMarketplace) =>
  item.location !== null && item.unsupported === null && item.existing_name === null;

export function ImportClaudeMarketplacesDialog({ open, ...props }: Props) {
  // Mounted afresh on every open, so the list is read again each time.
  return open ? <ImportClaudeMarketplacesBody {...props} /> : null;
}

function ImportClaudeMarketplacesBody({ onClose, onImported }: Omit<Props, "open">) {
  const { t } = useTranslation();
  const [data, setData] = useState<ClaudeMarketplaces | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const batch = useMarketplaceBatch();
  const { running, current, progress, outcomes } = batch;

  useEffect(() => {
    let stale = false;
    api
      .listClaudeCodeMarketplaces()
      .then((next) => {
        if (stale) return;
        setData(next);
        setSelected(new Set(next.marketplaces.filter(importable).map((item) => item.name)));
      })
      .catch((error) => {
        if (!stale) setLoadError(getErrorMessage(error, t("common.error")));
      });
    return () => {
      stale = true;
    };
  }, [t]);

  // Escape closes the dialog, but not while it is adding marketplaces.
  useEffect(() => {
    if (running) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, onClose]);

  const items = data?.marketplaces ?? [];
  // A failed one stays a candidate, so it can be tried again.
  const isDone = (name: string) => outcomes[name]?.status === "done";
  const candidates = items.filter((item) => importable(item) && !isDone(item.name));
  const queued = candidates.filter((item) => selected.has(item.name));
  const allSelected = candidates.length > 0 && candidates.every((item) => selected.has(item.name));

  const toggle = (name: string, checked: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (checked) next.add(name);
      else next.delete(name);
      return next;
    });
  };

  const handleImport = async () => {
    if (running || queued.length === 0) return;
    const result = await batch.run(
      queued.flatMap((item) =>
        item.location ? [{ key: item.name, location: item.location, name: item.name }] : []
      )
    );
    onImported(result.added);
    if (result.added.length > 0) {
      toast.success(t("install.marketplaces.importClaude.imported", { count: result.added.length }));
    }
    if (result.cancelled) {
      toast.info(t("install.toast.cancelled"));
    } else if (result.failed > 0) {
      toast.error(t("install.marketplaces.importClaude.failed", { count: result.failed }));
    } else {
      onClose();
    }
  };

  const handleCancel = () => {
    if (running) batch.cancel();
    else onClose();
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4">
      <div
        className="absolute inset-0 bg-black/70 backdrop-blur-sm"
        onClick={running ? undefined : onClose}
      />
      <div className="relative flex max-h-[calc(85vh/var(--app-scale))] w-full max-w-[560px] flex-col rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-2 flex items-center justify-between">
          <h2 className="text-[14px] font-semibold text-primary">
            {t("install.marketplaces.importClaude.title")}
          </h2>
          <button
            onClick={onClose}
            disabled={running}
            className="rounded-md p-1 text-muted outline-none transition-colors hover:text-secondary disabled:opacity-50"
            aria-label={t("install.marketplaces.importClaude.close")}
          >
            <X className="h-4 w-4" />
          </button>
        </div>
        <p className="mb-3 text-[13px] leading-5 text-tertiary">
          {t("install.marketplaces.importClaude.description", {
            path: data?.config_dir ?? "~/.claude",
          })}
        </p>

        {loadError ? (
          <p className="whitespace-pre-wrap break-words rounded-lg border border-red-500/25 bg-red-500/10 px-3 py-2 text-[12px] leading-5 text-red-600 dark:text-red-400">
            {loadError}
          </p>
        ) : data === null ? (
          <div className="flex items-center justify-center py-10">
            <Loader2 className="h-5 w-5 animate-spin text-muted" />
          </div>
        ) : items.length === 0 ? (
          <p className="py-8 text-center text-[13px] text-muted">
            {t("install.marketplaces.importClaude.empty")}
          </p>
        ) : (
          <>
            <div className="mb-2 flex items-center justify-between">
              <button
                type="button"
                onClick={() =>
                  setSelected(
                    allSelected ? new Set() : new Set(candidates.map((item) => item.name))
                  )
                }
                disabled={running || candidates.length === 0}
                className="text-[13px] font-medium text-accent-light hover:underline disabled:cursor-not-allowed disabled:no-underline disabled:opacity-50"
              >
                {allSelected
                  ? t("install.marketplaces.clearSelection")
                  : t("install.marketplaces.selectAll")}
              </button>
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto rounded-lg border border-border-subtle">
              {items.map((item) => {
                const outcome = outcomes[item.name];
                const isCurrent = current?.key === item.name;
                const canSelect = importable(item) && !isDone(item.name);
                return (
                  <label
                    key={item.name}
                    className={cn(
                      "flex items-start gap-3 border-b border-border-subtle px-3 py-2.5 last:border-b-0",
                      canSelect && !running ? "cursor-pointer hover:bg-surface-hover" : ""
                    )}
                  >
                    <input
                      type="checkbox"
                      checked={canSelect ? selected.has(item.name) : outcome?.status === "done"}
                      disabled={!canSelect || running}
                      onChange={(e) => toggle(item.name, e.target.checked)}
                      className="mt-0.5 h-4 w-4 shrink-0 accent-accent disabled:opacity-50"
                    />
                    <span className="min-w-0 flex-1">
                      <span className="flex min-w-0 flex-wrap items-center gap-1.5">
                        <span className="truncate text-[13px] font-semibold text-secondary">
                          {item.name}
                        </span>
                        <span className="rounded-full border border-border-subtle bg-background px-1.5 py-px text-[11px] text-muted">
                          {item.kind}
                        </span>
                        {item.existing_name ? (
                          <span className="inline-flex items-center gap-1 rounded-full border border-emerald-500/20 bg-emerald-500/10 px-1.5 py-px text-[11px] font-medium text-emerald-600 dark:text-emerald-400">
                            <Check className="h-3 w-3" />
                            {t("install.marketplaces.importClaude.alreadyAdded", {
                              name: item.existing_name,
                            })}
                          </span>
                        ) : null}
                        {outcome?.status === "done" ? (
                          <span className="inline-flex items-center gap-1 rounded-full border border-emerald-500/20 bg-emerald-500/10 px-1.5 py-px text-[11px] font-medium text-emerald-600 dark:text-emerald-400">
                            <Check className="h-3 w-3" />
                            {t("install.marketplaces.importClaude.added")}
                          </span>
                        ) : null}
                      </span>
                      {item.location ? (
                        <code
                          className="mt-0.5 block truncate text-[12px] text-muted"
                          title={item.location}
                        >
                          {item.location}
                        </code>
                      ) : null}
                      {item.unsupported ? (
                        <span className="mt-0.5 flex items-start gap-1 text-[12px] leading-5 text-amber-700 dark:text-amber-300">
                          <AlertTriangle className="mt-0.5 h-3 w-3 shrink-0" />
                          {t("install.marketplaces.importClaude.unavailable", {
                            reason: item.unsupported,
                          })}
                        </span>
                      ) : null}
                      {outcome?.status === "failed" ? (
                        <span className="mt-0.5 block break-words text-[12px] leading-5 text-red-600 dark:text-red-400">
                          {outcome.message}
                        </span>
                      ) : null}
                      {isCurrent && progress ? (
                        <span className="mt-0.5 block truncate text-[12px] text-muted" title={progress}>
                          {progress}
                        </span>
                      ) : null}
                    </span>
                    {isCurrent ? (
                      <Loader2 className="mt-0.5 h-3.5 w-3.5 shrink-0 animate-spin text-muted" />
                    ) : null}
                  </label>
                );
              })}
            </div>
          </>
        )}

        <div className="mt-4 flex items-center justify-between gap-3">
          <p className="min-w-0 truncate text-[12px] text-muted">
            {current
              ? t("install.marketplaces.importClaude.adding", {
                  name: current.key,
                  current: current.index,
                  total: current.total,
                })
              : null}
          </p>
          <div className="flex shrink-0 gap-2">
            <button
              type="button"
              onClick={handleCancel}
              className="px-3 py-1.5 text-[13px] font-medium text-muted transition-colors hover:text-secondary"
            >
              {running ? t("install.cancel") : t("install.marketplaces.importClaude.close")}
            </button>
            <button
              type="button"
              onClick={handleImport}
              disabled={running || queued.length === 0}
              className="app-button-primary px-3.5 py-2"
            >
              {running ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <Download className="h-3.5 w-3.5" />
              )}
              {t("install.marketplaces.importClaude.submit", { count: queued.length })}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
