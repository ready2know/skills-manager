import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronRight,
  Download,
  DownloadCloud,
  ExternalLink,
  FolderGit2,
  FolderOpen,
  Github,
  Loader2,
  Package,
  Pencil,
  Plus,
  RefreshCw,
  Search,
  Store,
  Trash2,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useNavigate } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { cn } from "../utils";
import { useApp } from "../context/AppContext";
import * as api from "../lib/tauri";
import type {
  Marketplace,
  MarketplaceDetail,
  MarketplaceInstallResult,
  MarketplacePlugin,
} from "../lib/tauri";
import { getErrorKind, getErrorMessage } from "../lib/error";
import { fileManagerNameKey } from "../lib/platform";
import { normalizeSkillSearch, skillSearchMatches } from "../lib/skillSearch";
import { AddMarketplaceDialog } from "./AddMarketplaceDialog";
import { ImportClaudeMarketplacesDialog } from "./ImportClaudeMarketplacesDialog";
import { ConfirmDialog } from "./ConfirmDialog";
import { StatusBanner } from "./StatusBanner";

const SELECTED_MARKETPLACE_KEY = "skills-manager.selectedMarketplaceId";
const NO_PATHS: ReadonlySet<string> = new Set();

/** A refresh or install in flight, and the marketplace it belongs to — which
 *  need not be the one on screen by the time it finishes. */
interface Operation {
  id: string;
  kind: "refresh" | "install";
  paths: ReadonlySet<string>;
}

function readSelectedId(): string | null {
  try {
    return localStorage.getItem(SELECTED_MARKETPLACE_KEY);
  } catch {
    return null;
  }
}

function writeSelectedId(id: string | null) {
  try {
    if (id) localStorage.setItem(SELECTED_MARKETPLACE_KEY, id);
    else localStorage.removeItem(SELECTED_MARKETPLACE_KEY);
  } catch {
    // Remembering the selection is a convenience; losing it is harmless.
  }
}

interface Props {
  /** Open the Git Install preview for a plugin hosted in another repository. */
  onBrowseRepository: (url: string) => void;
}

export function MarketplacesPanel({ onBrowseRepository }: Props) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { refreshPresets, refreshManagedSkills, openSkillDetailById } = useApp();
  const [marketplaces, setMarketplaces] = useState<Marketplace[] | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(readSelectedId);
  const [detail, setDetail] = useState<MarketplaceDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [operation, setOperation] = useState<Operation | null>(null);
  const [query, setQuery] = useState("");
  const [selection, setSelection] = useState<Set<string>>(new Set());
  const [addOpen, setAddOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [removeTarget, setRemoveTarget] = useState<Marketplace | null>(null);
  const [renameDraft, setRenameDraft] = useState<string | null>(null);
  const [warningsOpen, setWarningsOpen] = useState(false);
  const detailRequest = useRef(0);
  const selectedIdRef = useRef(selectedId);

  useEffect(() => {
    selectedIdRef.current = selectedId;
  }, [selectedId]);

  // One operation at a time: each holds the cancel key of its own marketplace.
  const busy = operation !== null;
  // Never show one marketplace's detail under another's selection.
  const current =
    detail && detail.marketplace.id === selectedId ? detail.marketplace : null;
  const ownOperation = operation && current && operation.id === current.id ? operation : null;
  const refreshing = ownOperation?.kind === "refresh";
  const installingPaths = ownOperation?.kind === "install" ? ownOperation.paths : NO_PATHS;

  const loadList = useCallback(async () => {
    try {
      const list = await api.listMarketplaces();
      setMarketplaces(list);
      setListError(null);
      return list;
    } catch (error) {
      setListError(getErrorMessage(error, t("common.error")));
      return null;
    }
  }, [t]);

  const loadDetail = useCallback(
    async (id: string) => {
      const request = ++detailRequest.current;
      setDetailLoading(true);
      setDetailError(null);
      try {
        const next = await api.getMarketplace(id);
        if (request !== detailRequest.current) return;
        setDetail(next);
        // A folder is re-read on open, which can change its counts.
        setMarketplaces((list) =>
          list?.map((item) => (item.id === id ? next.marketplace : item)) ?? list
        );
      } catch (error) {
        if (request !== detailRequest.current) return;
        setDetail(null);
        setDetailError(getErrorMessage(error, t("common.error")));
      } finally {
        if (request === detailRequest.current) setDetailLoading(false);
      }
    },
    [t]
  );

  useEffect(() => {
    loadList();
  }, [loadList]);

  // Keep the selection on a marketplace that exists.
  useEffect(() => {
    if (!marketplaces) return;
    if (selectedId && marketplaces.some((item) => item.id === selectedId)) return;
    setSelectedId(marketplaces[0]?.id ?? null);
  }, [marketplaces, selectedId]);

  useEffect(() => {
    writeSelectedId(selectedId);
    setQuery("");
    setSelection(new Set());
    setRenameDraft(null);
    setWarningsOpen(false);
    if (selectedId) {
      loadDetail(selectedId);
    } else {
      detailRequest.current += 1;
      setDetail(null);
      setDetailLoading(false);
    }
  }, [selectedId, loadDetail]);

  const applyDetail = (next: MarketplaceDetail) => {
    setDetail(next);
    setMarketplaces((list) =>
      list?.map((item) => (item.id === next.marketplace.id ? next.marketplace : item)) ?? list
    );
  };

  const withCloneProgress = async <T,>(
    key: string,
    toastId: string | number,
    run: () => Promise<T>
  ): Promise<T> => {
    const unlisten = await listen<{ skill_id: string; phase: string; detail?: string }>(
      "install-progress",
      (event) => {
        if (event.payload.skill_id !== key || event.payload.phase !== "cloning") return;
        const progress = event.payload.detail?.trim();
        toast.loading(
          progress ? `${t("install.toast.cloning")}\n${progress}` : t("install.toast.cloning"),
          { id: toastId }
        );
      }
    );
    try {
      return await run();
    } finally {
      unlisten();
    }
  };

  const handleRefresh = async () => {
    if (!current || busy) return;
    const { id, name } = current;
    setOperation({ id, kind: "refresh", paths: NO_PATHS });
    const toastId = toast.loading(t("install.marketplaces.refreshing", { name }));
    try {
      const next = await withCloneProgress(api.marketplaceOperationKey(id), toastId, () =>
        api.refreshMarketplace(id)
      );
      if (id === selectedIdRef.current) {
        applyDetail(next);
      } else {
        setMarketplaces((list) =>
          list?.map((item) => (item.id === id ? next.marketplace : item)) ?? list
        );
      }
      toast.success(t("install.marketplaces.refreshed", { name }), { id: toastId });
    } catch (error) {
      if (getErrorKind(error) === "cancelled") {
        toast.info(t("install.toast.cancelled"), { id: toastId });
      } else {
        toast.error(getErrorMessage(error, t("common.error")), { id: toastId });
        // The failure is recorded on the marketplace; show it.
        loadList();
        if (id === selectedIdRef.current) loadDetail(id);
      }
    } finally {
      setOperation(null);
    }
  };

  const goToSkill = (skillId: string) => {
    openSkillDetailById(skillId);
    navigate("/my-skills");
  };

  const reportInstall = (result: MarketplaceInstallResult, toastId: string | number) => {
    if (result.failed.length > 0) {
      const preview = result.failed.slice(0, 3).join("; ");
      const more = result.failed.length > 3 ? ` (+${result.failed.length - 3})` : "";
      toast.error(
        `${t("install.marketplaces.installFailed", { count: result.failed.length })}: ${preview}${more}`,
        { id: toastId }
      );
      return;
    }
    if (result.installed.length === 0) {
      toast.info(t("install.marketplaces.allSkipped", { count: result.skipped.length }), {
        id: toastId,
      });
      return;
    }
    toast.success(
      result.installed.length === 1
        ? t("install.toast.success", { name: result.installed[0] })
        : t("install.marketplaces.installedMany", { count: result.installed.length }),
      { id: toastId }
    );
  };

  const handleInstall = async (relPaths: string[]) => {
    if (!current || busy || relPaths.length === 0) return;
    const { id } = current;
    setOperation({ id, kind: "install", paths: new Set(relPaths) });
    const toastId = toast.loading(
      t("install.marketplaces.installing", { count: relPaths.length })
    );
    try {
      const result = await withCloneProgress(api.marketplaceOperationKey(id), toastId, () =>
        api.installMarketplaceSkills(id, relPaths)
      );
      reportInstall(result, toastId);
      setSelection(new Set());
    } catch (error) {
      if (getErrorKind(error) === "cancelled") {
        toast.info(t("install.toast.cancelled"), { id: toastId });
      } else {
        toast.error(getErrorMessage(error, t("common.error")), { id: toastId });
      }
    } finally {
      // Refresh even after a failure: a batch can stop part-way through.
      const results = await Promise.allSettled([
        refreshPresets(),
        refreshManagedSkills(),
        id === selectedIdRef.current ? loadDetail(id) : loadList(),
      ]);
      for (const r of results) {
        if (r.status === "rejected") console.warn("post-install refresh failed:", r.reason);
      }
      setOperation(null);
    }
  };

  const handleCancel = () => {
    if (!operation) return;
    api.cancelInstall(api.marketplaceOperationKey(operation.id)).catch(() => {
      // The operation may have finished before the cancel arrived.
    });
  };

  const handleRename = async () => {
    if (!current || renameDraft === null) return;
    const name = renameDraft.trim();
    if (!name || name === current.name) {
      setRenameDraft(null);
      return;
    }
    try {
      const renamed = await api.renameMarketplace(current.id, name);
      setDetail((prev) => (prev ? { ...prev, marketplace: renamed } : prev));
      setMarketplaces((list) =>
        list?.map((item) => (item.id === renamed.id ? renamed : item)) ?? list
      );
      setRenameDraft(null);
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    }
  };

  const handleRemove = async () => {
    if (!removeTarget) return;
    const { id, name } = removeTarget;
    try {
      await api.removeMarketplace(id);
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
      return;
    }
    toast.success(t("install.marketplaces.removed", { name }));
    await loadList();
  };

  const handleReveal = async () => {
    if (!current) return;
    try {
      await api.revealMarketplaceFolder(current.id);
    } catch (error) {
      toast.error(getErrorMessage(error, t("common.error")));
    }
  };

  const handleAdded = (added: MarketplaceDetail[]) => {
    if (added.length === 0) return;
    setMarketplaces((list) => [...(list ?? []), ...added.map((detail) => detail.marketplace)]);
    setSelectedId(added[0].marketplace.id);
  };

  const normalizedQuery = normalizeSkillSearch(query);
  const visiblePlugins = useMemo(() => {
    if (!detail || detail.marketplace.id !== selectedId) return [];
    return detail.plugins
      .map((plugin) => {
        const pluginMatches = skillSearchMatches(normalizedQuery, [
          plugin.name,
          plugin.description,
          plugin.category,
        ]);
        const skills = pluginMatches
          ? plugin.skills
          : plugin.skills.filter((skill) =>
              skillSearchMatches(normalizedQuery, [skill.name, skill.description, skill.rel_path])
            );
        return { ...plugin, skills, pluginMatches };
      })
      .filter((plugin) => plugin.pluginMatches || plugin.skills.length > 0);
  }, [detail, selectedId, normalizedQuery]);

  const selectableVisible = useMemo(
    () =>
      visiblePlugins
        .flatMap((plugin) => plugin.skills)
        .filter((skill) => !skill.installed_skill_id)
        .map((skill) => skill.rel_path),
    [visiblePlugins]
  );
  const allVisibleSelected =
    selectableVisible.length > 0 && selectableVisible.every((path) => selection.has(path));

  const toggleSelection = (relPath: string, checked: boolean) => {
    setSelection((prev) => {
      const next = new Set(prev);
      if (checked) next.add(relPath);
      else next.delete(relPath);
      return next;
    });
  };

  const toggleAllVisible = () => {
    setSelection((prev) => {
      const next = new Set(prev);
      if (allVisibleSelected) selectableVisible.forEach((path) => next.delete(path));
      else selectableVisible.forEach((path) => next.add(path));
      return next;
    });
  };

  if (marketplaces === null) {
    return listError ? (
      <StatusBanner
        compact
        title={t("common.requestFailed")}
        description={listError}
        actionLabel={t("common.retry")}
        onAction={loadList}
        tone="danger"
      />
    ) : (
      <div className="flex items-center justify-center py-16">
        <Loader2 className="h-5 w-5 animate-spin text-muted" />
      </div>
    );
  }

  const addDialog = (
    <>
      <AddMarketplaceDialog
        open={addOpen}
        onClose={() => setAddOpen(false)}
        onAdded={handleAdded}
      />
      <ImportClaudeMarketplacesDialog
        open={importOpen}
        onClose={() => setImportOpen(false)}
        onImported={handleAdded}
      />
    </>
  );

  if (marketplaces.length === 0) {
    return (
      <div className="pb-8">
        <div className="app-panel flex flex-col items-center justify-center px-6 py-14 text-center">
          <div className="flex h-12 w-12 items-center justify-center rounded-xl border border-border bg-background text-muted">
            <Store className="h-5 w-5" />
          </div>
          <h3 className="mt-4 text-[14px] font-semibold text-secondary">
            {t("install.marketplaces.emptyTitle")}
          </h3>
          <p className="mt-1 max-w-lg text-[13px] leading-5 text-muted">
            {t("install.marketplaces.emptyDescription")}
          </p>
          <div className="mt-5 flex flex-wrap justify-center gap-2">
            <button
              type="button"
              onClick={() => setAddOpen(true)}
              className="app-button-primary"
            >
              <Plus className="h-4 w-4" />
              {t("install.marketplaces.add")}
            </button>
            <button
              type="button"
              onClick={() => setImportOpen(true)}
              className="app-button-secondary bg-background"
            >
              <Download className="h-4 w-4" />
              {t("install.marketplaces.importClaude.button")}
            </button>
          </div>
        </div>
        {addDialog}
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3 pb-8 lg:min-h-0 lg:flex-[1_1_0px] lg:flex-row lg:pb-0">
      <aside className="app-panel flex shrink-0 flex-col overflow-hidden lg:w-72">
        <div className="flex shrink-0 items-center justify-between gap-2 border-b border-border-subtle px-3 py-2.5">
          <span className="app-section-title">{t("install.marketplaces.title")}</span>
          <div className="flex items-center gap-0.5">
            <button
              type="button"
              onClick={() => setImportOpen(true)}
              className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-[12px] font-medium text-muted transition-colors hover:bg-surface-hover hover:text-secondary"
              title={t("install.marketplaces.importClaude.button")}
            >
              <Download className="h-3.5 w-3.5" />
              {t("install.marketplaces.importClaude.short")}
            </button>
            <button
              type="button"
              onClick={() => setAddOpen(true)}
              className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-[12px] font-medium text-accent-light transition-colors hover:bg-accent-bg"
              title={t("install.marketplaces.add")}
            >
              <Plus className="h-3.5 w-3.5" />
              {t("install.marketplaces.addShort")}
            </button>
          </div>
        </div>
        <div className="max-h-[420px] min-h-0 flex-1 overflow-y-auto scrollbar-hide lg:max-h-none">
          {marketplaces.map((item) => {
            const isSelected = item.id === selectedId;
            const Icon = item.kind === "local" ? FolderGit2 : Github;
            return (
              <button
                key={item.id}
                type="button"
                onClick={() => setSelectedId(item.id)}
                aria-pressed={isSelected}
                className={cn(
                  "flex w-full items-start gap-2.5 border-b border-border-subtle px-3 py-2.5 text-left outline-none transition-colors last:border-b-0",
                  isSelected ? "bg-accent-bg" : "hover:bg-surface-hover"
                )}
              >
                <span
                  className={cn(
                    "mt-0.5 flex h-7 w-7 shrink-0 items-center justify-center rounded-md border",
                    isSelected
                      ? "border-accent-border text-accent-light"
                      : "border-border-subtle bg-background text-muted"
                  )}
                >
                  <Icon className="h-3.5 w-3.5" />
                </span>
                <span className="min-w-0 flex-1">
                  <span
                    className={cn(
                      "block truncate text-[13px] font-semibold",
                      isSelected ? "text-accent-light" : "text-secondary"
                    )}
                  >
                    {item.name}
                  </span>
                  <span className="block truncate text-[12px] text-muted">
                    {t("install.marketplaces.skillCount", { count: item.skill_count })}
                    {" · "}
                    {t("install.marketplaces.installedCount", { count: item.installed_count })}
                  </span>
                </span>
                {item.last_error ? (
                  <AlertTriangle
                    className="mt-1 h-3.5 w-3.5 shrink-0 text-amber-600 dark:text-amber-400"
                    aria-label={t("install.marketplaces.errorTitle")}
                  />
                ) : null}
              </button>
            );
          })}
        </div>
      </aside>

      <section className="min-w-0 flex-1 space-y-3 scrollbar-hide lg:overflow-y-auto">
        {detailLoading && !current ? (
          <div className="flex items-center justify-center py-16">
            <Loader2 className="h-5 w-5 animate-spin text-muted" />
          </div>
        ) : detailError && !current ? (
          <StatusBanner
            compact
            title={t("common.requestFailed")}
            description={detailError}
            actionLabel={t("common.retry")}
            onAction={() => selectedId && loadDetail(selectedId)}
            tone="danger"
          />
        ) : detail && current ? (
          <>
            <div className="app-panel p-4">
              <div className="flex flex-col gap-3 md:flex-row md:items-start md:justify-between">
                <div className="min-w-0 flex-1">
                  {renameDraft !== null ? (
                    <input
                      autoFocus
                      value={renameDraft}
                      onChange={(e) => setRenameDraft(e.target.value)}
                      onBlur={handleRename}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                        else if (e.key === "Escape") setRenameDraft(null);
                      }}
                      aria-label={t("common.rename")}
                      className="app-input h-8 w-full max-w-sm bg-background text-[14px] font-semibold"
                    />
                  ) : (
                    <div className="flex min-w-0 items-center gap-1.5">
                      <h2 className="truncate text-[14px] font-semibold text-primary">
                        {current.name}
                      </h2>
                      <button
                        type="button"
                        onClick={() => setRenameDraft(current.name)}
                        className="shrink-0 rounded-md p-1 text-muted transition-colors hover:bg-surface-hover hover:text-secondary"
                        title={t("common.rename")}
                      >
                        <Pencil className="h-3 w-3" />
                      </button>
                    </div>
                  )}

                  <div className="mt-2 flex flex-wrap items-center gap-1.5">
                    <span className="inline-flex items-center gap-1 rounded-full border border-border-subtle bg-background px-2 py-0.5 text-[11px] font-medium text-tertiary">
                      {current.kind === "local" ? (
                        <FolderGit2 className="h-3 w-3" />
                      ) : (
                        <Github className="h-3 w-3" />
                      )}
                      {t(
                        current.kind === "local"
                          ? "install.marketplaces.kindLocal"
                          : "install.marketplaces.kindRemote"
                      )}
                    </span>
                    {current.format ? (
                      <span className="rounded-full border border-border-subtle bg-background px-2 py-0.5 text-[11px] font-medium text-tertiary">
                        {t(
                          current.format === "claude_plugins"
                            ? "install.marketplaces.formatPlugins"
                            : "install.marketplaces.formatFolders"
                        )}
                      </span>
                    ) : null}
                    <span className="rounded-full border border-border-subtle bg-background px-2 py-0.5 text-[11px] font-medium text-tertiary">
                      {[
                        current.format === "claude_plugins"
                          ? t("install.marketplaces.pluginCount", { count: current.plugin_count })
                          : null,
                        t("install.marketplaces.skillCount", { count: current.skill_count }),
                        t("install.marketplaces.installedCount", {
                          count: current.installed_count,
                        }),
                      ]
                        .filter(Boolean)
                        .join(" · ")}
                    </span>
                    {current.branch ? (
                      <span className="rounded-full border border-border-subtle bg-background px-2 py-0.5 font-mono text-[11px] text-tertiary">
                        {current.branch}
                      </span>
                    ) : null}
                    {current.revision ? (
                      <span
                        className="rounded-full border border-border-subtle bg-background px-2 py-0.5 font-mono text-[11px] text-tertiary"
                        title={current.revision}
                      >
                        {current.revision.slice(0, 7)}
                      </span>
                    ) : null}
                  </div>

                  <code
                    className="mt-2 block truncate text-[12px] text-muted"
                    title={current.location}
                  >
                    {current.location}
                  </code>
                  {current.description ? (
                    <p className="mt-1.5 text-[13px] leading-5 text-tertiary">
                      {current.description}
                    </p>
                  ) : null}
                  {current.last_synced_at ? (
                    <p className="mt-1.5 text-[12px] text-muted">
                      {t("install.marketplaces.lastSynced", {
                        time: new Date(current.last_synced_at).toLocaleString(),
                      })}
                    </p>
                  ) : null}
                </div>

                <div className="flex shrink-0 flex-wrap items-center gap-1.5">
                  {refreshing ? (
                    <button
                      type="button"
                      onClick={handleCancel}
                      className="inline-flex items-center gap-1.5 rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-1.5 text-[13px] font-medium text-red-600 transition-colors hover:bg-red-500/20 dark:text-red-400"
                    >
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                      {t("install.cancel")}
                    </button>
                  ) : (
                    <button
                      type="button"
                      onClick={handleRefresh}
                      disabled={busy}
                      className="inline-flex items-center gap-1.5 rounded-lg border border-border-subtle bg-background px-3 py-1.5 text-[13px] font-medium text-secondary transition-colors hover:bg-surface-hover disabled:opacity-50"
                      title={t(
                        current.kind === "local"
                          ? "install.marketplaces.refreshLocalHint"
                          : "install.marketplaces.refreshRemoteHint"
                      )}
                    >
                      <RefreshCw className="h-3.5 w-3.5" />
                      {t("install.marketplaces.refresh")}
                    </button>
                  )}
                  {current.kind === "local" ? (
                    <button
                      type="button"
                      onClick={handleReveal}
                      className="rounded-lg border border-border-subtle bg-background p-2 text-muted transition-colors hover:bg-surface-hover hover:text-secondary"
                      title={t("project.revealInFileManager", { app: t(fileManagerNameKey) })}
                    >
                      <FolderOpen className="h-3.5 w-3.5" />
                    </button>
                  ) : current.web_url ? (
                    <button
                      type="button"
                      onClick={() => current.web_url && openUrl(current.web_url)}
                      className="rounded-lg border border-border-subtle bg-background p-2 text-muted transition-colors hover:bg-surface-hover hover:text-secondary"
                      title={t("install.marketplaces.openWeb")}
                    >
                      <ExternalLink className="h-3.5 w-3.5" />
                    </button>
                  ) : null}
                  <button
                    type="button"
                    onClick={() => setRemoveTarget(current)}
                    disabled={busy}
                    className="rounded-lg border border-border-subtle bg-background p-2 text-muted transition-colors hover:border-red-500/30 hover:bg-red-500/10 hover:text-red-600 disabled:opacity-50 dark:hover:text-red-400"
                    title={t("install.marketplaces.remove")}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </button>
                </div>
              </div>
            </div>

            {current.last_error ? (
              <StatusBanner
                compact
                title={t("install.marketplaces.errorTitle")}
                description={current.last_error}
                actionLabel={busy ? undefined : t("common.retry")}
                onAction={handleRefresh}
                tone="danger"
              />
            ) : null}

            {detail.warnings.length > 0 ? (
              <div className="rounded-xl border border-amber-500/25 bg-amber-500/10 px-3.5 py-2.5">
                <button
                  type="button"
                  onClick={() => setWarningsOpen((open) => !open)}
                  aria-expanded={warningsOpen}
                  className="flex w-full items-center gap-2 text-left text-[13px] font-medium text-amber-700 dark:text-amber-300"
                >
                  {warningsOpen ? (
                    <ChevronDown className="h-3.5 w-3.5 shrink-0" />
                  ) : (
                    <ChevronRight className="h-3.5 w-3.5 shrink-0" />
                  )}
                  {t("install.marketplaces.warningsTitle", { count: detail.warnings.length })}
                </button>
                {warningsOpen ? (
                  <ul className="mt-2 space-y-1 pl-5 text-[12px] leading-5 text-muted">
                    {detail.warnings.map((warning) => (
                      <li key={warning} className="list-disc break-words">
                        {warning}
                      </li>
                    ))}
                  </ul>
                ) : null}
              </div>
            ) : null}

            <div className="app-panel flex flex-col gap-2 p-2.5 md:flex-row md:items-center">
              <div className="relative min-w-0 flex-1">
                <Search className="pointer-events-none absolute left-3 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted" />
                <input
                  type="text"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  placeholder={t("install.marketplaces.searchPlaceholder")}
                  className="app-input w-full bg-background pl-9"
                  autoCapitalize="none"
                  autoCorrect="off"
                  spellCheck={false}
                />
              </div>
              <div className="flex shrink-0 items-center gap-2">
                <button
                  type="button"
                  onClick={toggleAllVisible}
                  disabled={selectableVisible.length === 0 || busy}
                  className="px-1.5 text-[13px] font-medium text-accent-light hover:underline disabled:cursor-not-allowed disabled:no-underline disabled:opacity-50"
                >
                  {allVisibleSelected
                    ? t("install.marketplaces.clearSelection")
                    : t("install.marketplaces.selectAll")}
                </button>
                {installingPaths.size > 0 && current.kind === "remote" ? (
                  <button
                    type="button"
                    onClick={handleCancel}
                    className="inline-flex h-10 items-center gap-1.5 rounded-lg border border-red-500/30 bg-red-500/10 px-3 text-[13px] font-medium text-red-600 transition-colors hover:bg-red-500/20 dark:text-red-400"
                  >
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    {t("install.cancel")}
                  </button>
                ) : (
                  <button
                    type="button"
                    onClick={() => handleInstall([...selection])}
                    disabled={selection.size === 0 || busy}
                    className="app-toolbar-button app-toolbar-button-primary disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    {installingPaths.size > 0 ? (
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    ) : (
                      <DownloadCloud className="h-3.5 w-3.5" />
                    )}
                    {t("install.marketplaces.installSelected", { count: selection.size })}
                  </button>
                )}
              </div>
            </div>

            {detail.plugins.length === 0 ? (
              <EmptyState
                title={t("install.marketplaces.noSkillsTitle")}
                description={t("install.marketplaces.noSkillsDescription")}
              />
            ) : visiblePlugins.length === 0 ? (
              <EmptyState
                title={t("install.marketplaces.noResults")}
                description={t("install.noResults.description")}
              />
            ) : (
              visiblePlugins.map((plugin) => (
                <PluginSection
                  key={plugin.name || "__root__"}
                  plugin={plugin}
                  selection={selection}
                  installingPaths={installingPaths}
                  busy={busy}
                  onToggle={toggleSelection}
                  onInstall={(relPath) => handleInstall([relPath])}
                  onOpenSkill={goToSkill}
                  onBrowseRepository={onBrowseRepository}
                />
              ))
            )}
          </>
        ) : null}
      </section>

      {addDialog}
      <ConfirmDialog
        open={removeTarget !== null}
        title={t("install.marketplaces.remove")}
        message={t("install.marketplaces.removeConfirm", { name: removeTarget?.name ?? "" })}
        confirmLabel={t("common.delete")}
        onClose={() => setRemoveTarget(null)}
        onConfirm={handleRemove}
      />
    </div>
  );
}

function EmptyState({ title, description }: { title: string; description: string }) {
  return (
    <div className="app-panel flex flex-col items-center justify-center px-6 py-12 text-center">
      <div className="flex h-10 w-10 items-center justify-center rounded-lg border border-border bg-background text-muted">
        <Search className="h-4 w-4" />
      </div>
      <h3 className="mt-3 text-[13px] font-semibold text-secondary">{title}</h3>
      <p className="mt-1 max-w-md text-[13px] text-muted">{description}</p>
    </div>
  );
}

interface PluginSectionProps {
  plugin: MarketplacePlugin;
  selection: ReadonlySet<string>;
  installingPaths: ReadonlySet<string>;
  busy: boolean;
  onToggle: (relPath: string, checked: boolean) => void;
  onInstall: (relPath: string) => void;
  onOpenSkill: (skillId: string) => void;
  onBrowseRepository: (url: string) => void;
}

function PluginSection({
  plugin,
  selection,
  installingPaths,
  busy,
  onToggle,
  onInstall,
  onOpenSkill,
  onBrowseRepository,
}: PluginSectionProps) {
  const { t } = useTranslation();
  const external = plugin.external;

  return (
    <section className="app-panel overflow-hidden">
      {plugin.name ? (
        <header className="flex items-start justify-between gap-3 border-b border-border-subtle px-4 py-2.5 last:border-b-0">
          <div className="min-w-0">
            <div className="flex min-w-0 flex-wrap items-center gap-1.5">
              <Package className="h-3.5 w-3.5 shrink-0 text-muted" />
              <h3 className="truncate text-[13px] font-semibold text-secondary">{plugin.name}</h3>
              {plugin.version ? (
                <span className="rounded-full border border-border-subtle bg-background px-1.5 py-px font-mono text-[11px] text-muted">
                  v{plugin.version}
                </span>
              ) : null}
              {plugin.category ? (
                <span className="rounded-full border border-border-subtle bg-background px-1.5 py-px text-[11px] text-muted">
                  {plugin.category}
                </span>
              ) : null}
            </div>
            {plugin.description ? (
              <p className="mt-0.5 line-clamp-2 text-[12px] leading-5 text-muted">
                {plugin.description}
              </p>
            ) : null}
            {external ? (
              <p className="mt-1 text-[12px] text-muted">
                {external.url
                  ? t("install.marketplaces.externalPlugin")
                  : t("install.marketplaces.externalUnsupported", { kind: external.kind })}
              </p>
            ) : plugin.skills.length === 0 ? (
              <p className="mt-1 text-[12px] text-muted">
                {t("install.marketplaces.pluginWithoutSkills")}
              </p>
            ) : null}
          </div>
          {external?.url ? (
            <button
              type="button"
              onClick={() => external.url && onBrowseRepository(external.url)}
              disabled={busy}
              className="inline-flex shrink-0 items-center gap-1.5 rounded-lg border border-border-subtle bg-background px-2.5 py-1.5 text-[12px] font-medium text-secondary transition-colors hover:bg-surface-hover disabled:opacity-50"
              title={external.url}
            >
              <Github className="h-3.5 w-3.5" />
              {t("install.marketplaces.externalBrowse")}
            </button>
          ) : plugin.skills.length > 0 ? (
            <span className="shrink-0 text-[12px] text-muted">
              {t("install.marketplaces.skillCount", { count: plugin.skills.length })}
            </span>
          ) : null}
        </header>
      ) : null}

      {plugin.skills.map((skill) => {
        const installed = Boolean(skill.installed_skill_id);
        const installing = installingPaths.has(skill.rel_path);
        return (
          <div
            key={skill.rel_path}
            className="flex items-start gap-3 border-b border-border-subtle px-4 py-2.5 last:border-b-0"
          >
            <input
              type="checkbox"
              checked={installed || selection.has(skill.rel_path)}
              disabled={installed || busy}
              onChange={(e) => onToggle(skill.rel_path, e.target.checked)}
              className="mt-0.5 h-4 w-4 shrink-0 accent-accent disabled:opacity-50"
              aria-label={skill.name}
            />
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                <span className="truncate text-[13px] font-semibold text-secondary">
                  {skill.name}
                </span>
                {installed ? (
                  <span className="inline-flex items-center gap-1 rounded-full border border-emerald-500/20 bg-emerald-500/10 px-1.5 py-px text-[11px] font-medium text-emerald-600 dark:text-emerald-400">
                    <Check className="h-3 w-3" />
                    {skill.installed_name && skill.installed_name !== skill.name
                      ? t("install.marketplaces.installedAs", { name: skill.installed_name })
                      : t("install.installed")}
                  </span>
                ) : null}
                {skill.update_status === "update_available" ? (
                  <span className="rounded-full border border-amber-500/25 bg-amber-500/10 px-1.5 py-px text-[11px] font-medium text-amber-700 dark:text-amber-300">
                    {t("install.marketplaces.updateAvailable")}
                  </span>
                ) : null}
              </div>
              {skill.description ? (
                <p className="mt-0.5 line-clamp-2 text-[12px] leading-5 text-muted">
                  {skill.description}
                </p>
              ) : null}
              <code className="mt-0.5 block truncate text-[11px] text-faint" title={skill.rel_path}>
                {skill.rel_path}
              </code>
            </div>
            {installed && skill.installed_skill_id ? (
              <button
                type="button"
                onClick={() => skill.installed_skill_id && onOpenSkill(skill.installed_skill_id)}
                className="shrink-0 rounded-md px-2 py-1 text-[12px] font-medium text-accent-light transition-colors hover:bg-accent-bg"
              >
                {t("install.marketplaces.openInLibrary")}
              </button>
            ) : installing ? (
              <span className="shrink-0 p-1">
                <Loader2 className="h-3.5 w-3.5 animate-spin text-muted" />
              </span>
            ) : (
              <button
                type="button"
                onClick={() => onInstall(skill.rel_path)}
                disabled={busy}
                className="shrink-0 rounded-md border border-accent-border bg-accent-dark p-1 text-white transition-colors hover:bg-accent disabled:opacity-50"
                title={t("install.oneClickInstall")}
                aria-label={`${t("install.oneClickInstall")} ${skill.name}`}
              >
                <Plus className="h-3.5 w-3.5" />
              </button>
            )}
          </div>
        );
      })}
    </section>
  );
}
