import { useEffect, useState } from "react";
import { Check, FolderOpen, Loader2, X } from "lucide-react";
import { useTranslation } from "react-i18next";
import { open as dialogOpen } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";
import { cn } from "../utils";
import type { MarketplaceDetail } from "../lib/tauri";
import { useMarketplaceBatch } from "../hooks/useMarketplaceBatch";

interface Props {
  open: boolean;
  onClose: () => void;
  /** Called once a run ends, with every marketplace it added. */
  onAdded: (added: MarketplaceDetail[]) => void;
}

/** The non-empty lines of `text`, trimmed, each once. */
function parseLocations(text: string): string[] {
  const seen = new Set<string>();
  for (const line of text.split(/\r?\n/)) {
    const location = line.trim();
    if (location) seen.add(location);
  }
  return [...seen];
}

export function AddMarketplaceDialog({ open, ...props }: Props) {
  // Mounted afresh on every open, so each one starts empty.
  return open ? <AddMarketplaceDialogBody {...props} /> : null;
}

function AddMarketplaceDialogBody({ onClose, onAdded }: Omit<Props, "open">) {
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [name, setName] = useState("");
  // The locations of the last run, kept to show how each one went.
  const [runItems, setRunItems] = useState<string[]>([]);
  const batch = useMarketplaceBatch();

  // Escape closes the dialog, but not while it is adding marketplaces.
  useEffect(() => {
    if (batch.running) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [batch.running, onClose]);

  const locations = parseLocations(text);
  const single = locations.length === 1;

  const handleChooseFolders = async () => {
    const picked = await dialogOpen({ directory: true, multiple: true });
    if (!picked) return;
    const paths = Array.isArray(picked) ? picked : [picked];
    setText((prev) => parseLocations([prev, ...paths].join("\n")).join("\n"));
  };

  const handleAdd = async () => {
    if (locations.length === 0 || batch.running) return;
    setRunItems(locations);
    const result = await batch.run(
      locations.map((location) => ({
        key: location,
        location,
        name: single ? name.trim() || undefined : undefined,
      }))
    );
    onAdded(result.added);

    if (result.added.length === 1) {
      toast.success(t("install.marketplaces.added", { name: result.added[0].marketplace.name }));
    } else if (result.added.length > 1) {
      toast.success(t("install.marketplaces.addDialog.addedMany", { count: result.added.length }));
    }
    // What was not added stays in the box, to fix and try again.
    const remaining = locations.filter((location) => !result.done.includes(location));
    setText(remaining.join("\n"));
    if (result.cancelled) {
      toast.info(t("install.toast.cancelled"));
    } else if (result.failed > 0) {
      toast.error(t("install.marketplaces.addDialog.failedMany", { count: result.failed }));
    } else {
      onClose();
    }
  };

  const handleCancel = () => {
    if (batch.running) batch.cancel();
    else onClose();
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4">
      <div
        className="absolute inset-0 bg-black/70 backdrop-blur-sm"
        onClick={batch.running ? undefined : onClose}
      />
      <div className="relative flex max-h-[calc(85vh/var(--app-scale))] w-full max-w-[560px] flex-col rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-[14px] font-semibold text-primary">
            {t("install.marketplaces.addDialog.title")}
          </h2>
          <button
            onClick={onClose}
            disabled={batch.running}
            className="rounded-md p-1 text-muted outline-none transition-colors hover:text-secondary disabled:opacity-50"
            aria-label={t("common.cancel")}
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        <div className="min-h-0 flex-1 space-y-3 overflow-y-auto">
          <ul className="space-y-1 pl-4 text-[12px] leading-5 text-tertiary">
            <li className="list-disc">{t("install.marketplaces.addDialog.remoteHint")}</li>
            <li className="list-disc">{t("install.marketplaces.addDialog.localHint")}</li>
          </ul>

          <div>
            <div className="mb-1 flex items-end justify-between gap-2">
              <label
                htmlFor="marketplace-locations"
                className="text-[12px] font-medium text-tertiary"
              >
                {t("install.marketplaces.addDialog.locations")}
              </label>
              <button
                type="button"
                onClick={handleChooseFolders}
                disabled={batch.running}
                className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-[12px] font-medium text-accent-light transition-colors hover:bg-accent-bg disabled:opacity-50"
              >
                <FolderOpen className="h-3.5 w-3.5" />
                {t("install.marketplaces.addDialog.chooseFolders")}
              </button>
            </div>
            <textarea
              id="marketplace-locations"
              value={text}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  handleAdd();
                }
              }}
              placeholder={t("install.marketplaces.addDialog.locationsPlaceholder")}
              disabled={batch.running}
              // Grows with the list, like the app's other scroll areas without a visible bar.
              rows={Math.min(Math.max(text.split("\n").length, 4), 10)}
              className="scrollbar-hide w-full resize-none rounded-lg border border-border-subtle bg-background px-3 py-2 font-mono text-[12px] leading-5 text-secondary outline-none transition-colors placeholder:text-faint focus:border-border disabled:opacity-60"
              autoFocus
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
            />
          </div>

          {single && !batch.running ? (
            <div>
              <label
                htmlFor="marketplace-name"
                className="mb-1 block text-[12px] font-medium text-tertiary"
              >
                {t("install.marketplaces.addDialog.name")}
              </label>
              <input
                id="marketplace-name"
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && handleAdd()}
                placeholder={t("install.marketplaces.addDialog.namePlaceholder")}
                className="app-input w-full bg-background"
              />
            </div>
          ) : null}

          {runItems.length > 0 ? (
            <ul className="overflow-hidden rounded-lg border border-border-subtle">
              {runItems.map((location) => {
                const outcome = batch.outcomes[location];
                const isCurrent = batch.current?.key === location;
                return (
                  <li
                    key={location}
                    className="flex items-start gap-2.5 border-b border-border-subtle px-3 py-2 last:border-b-0"
                  >
                    <span className="mt-0.5 flex h-3.5 w-3.5 shrink-0 items-center justify-center">
                      {isCurrent ? (
                        <Loader2 className="h-3.5 w-3.5 animate-spin text-muted" />
                      ) : outcome?.status === "done" ? (
                        <Check className="h-3.5 w-3.5 text-emerald-600 dark:text-emerald-400" />
                      ) : outcome?.status === "failed" ? (
                        <X className="h-3.5 w-3.5 text-red-600 dark:text-red-400" />
                      ) : (
                        <span className="h-1.5 w-1.5 rounded-full bg-border" />
                      )}
                    </span>
                    <span className="min-w-0 flex-1">
                      <code
                        className={cn(
                          "block truncate text-[12px]",
                          outcome?.status === "done" ? "text-muted" : "text-secondary"
                        )}
                        title={location}
                      >
                        {location}
                      </code>
                      {outcome?.status === "done" ? (
                        <span className="block truncate text-[12px] text-muted">
                          {t("install.marketplaces.added", {
                            name: outcome.detail.marketplace.name,
                          })}
                        </span>
                      ) : null}
                      {outcome?.status === "failed" ? (
                        <span className="block whitespace-pre-wrap break-words text-[12px] leading-5 text-red-600 dark:text-red-400">
                          {outcome.message}
                        </span>
                      ) : null}
                      {isCurrent && batch.progress ? (
                        <span className="block truncate text-[12px] text-muted" title={batch.progress}>
                          {batch.progress}
                        </span>
                      ) : null}
                    </span>
                  </li>
                );
              })}
            </ul>
          ) : null}
        </div>

        <div className="mt-4 flex items-center justify-between gap-3">
          <p className="min-w-0 truncate text-[12px] text-muted">
            {batch.current && batch.current.total > 1
              ? t("install.marketplaces.addDialog.progress", {
                  current: batch.current.index,
                  total: batch.current.total,
                })
              : null}
          </p>
          <div className="flex shrink-0 gap-2">
            <button
              type="button"
              onClick={handleCancel}
              className="px-3 py-1.5 text-[13px] font-medium text-muted transition-colors hover:text-secondary"
            >
              {batch.running ? t("install.cancel") : t("common.cancel")}
            </button>
            <button
              type="button"
              onClick={handleAdd}
              disabled={locations.length === 0 || batch.running}
              className="app-button-primary px-3.5 py-2"
            >
              {batch.running ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : null}
              {batch.running
                ? t("install.marketplaces.addDialog.adding")
                : locations.length > 1
                  ? t("install.marketplaces.addDialog.submitMany", { count: locations.length })
                  : t("install.marketplaces.addDialog.submit")}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
