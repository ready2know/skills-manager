import { useCallback, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import * as api from "../lib/tauri";
import type { MarketplaceDetail } from "../lib/tauri";
import { getErrorKind, getErrorMessage } from "../lib/error";

export interface MarketplaceBatchItem {
  /** Identifies the row the outcome belongs to. */
  key: string;
  location: string;
  name?: string;
}

export type MarketplaceBatchOutcome =
  | { status: "done"; detail: MarketplaceDetail }
  | { status: "failed"; message: string };

export interface MarketplaceBatchResult {
  added: MarketplaceDetail[];
  /** Keys of the items that were added, in order. */
  done: string[];
  failed: number;
  cancelled: boolean;
}

/**
 * Adds marketplaces one after another, reporting which one is being cloned
 * and how far along, and stopping cleanly on cancel.
 *
 * One at a time on purpose: each add clones a repository, and clones run in
 * parallel would compete for the same network and repo cache while the user
 * watches several progress lines at once.
 */
export function useMarketplaceBatch() {
  const { t } = useTranslation();
  const [running, setRunning] = useState(false);
  const [current, setCurrent] = useState<{ key: string; index: number; total: number } | null>(
    null
  );
  const [progress, setProgress] = useState<string | null>(null);
  const [outcomes, setOutcomes] = useState<Record<string, MarketplaceBatchOutcome>>({});
  const operationKey = useRef<string | null>(null);
  const cancelRequested = useRef(false);

  const reset = useCallback(() => {
    setOutcomes({});
    setCurrent(null);
    setProgress(null);
  }, []);

  const run = useCallback(
    async (items: MarketplaceBatchItem[]): Promise<MarketplaceBatchResult> => {
      const result: MarketplaceBatchResult = { added: [], done: [], failed: 0, cancelled: false };
      if (items.length === 0) return result;
      setRunning(true);
      cancelRequested.current = false;
      const unlisten = await listen<{ skill_id: string; phase: string; detail?: string }>(
        "install-progress",
        (event) => {
          if (event.payload.skill_id !== operationKey.current) return;
          const detail = event.payload.detail?.trim();
          if (detail) setProgress(detail);
        }
      );
      try {
        for (const [index, item] of items.entries()) {
          if (cancelRequested.current) {
            result.cancelled = true;
            break;
          }
          operationKey.current = api.marketplaceOperationKey(item.location);
          setCurrent({ key: item.key, index: index + 1, total: items.length });
          setProgress(null);
          setOutcomes((prev) => {
            const next = { ...prev };
            delete next[item.key];
            return next;
          });
          try {
            const detail = await api.addMarketplace(item.location, item.name);
            result.added.push(detail);
            result.done.push(item.key);
            setOutcomes((prev) => ({ ...prev, [item.key]: { status: "done", detail } }));
          } catch (error) {
            if (getErrorKind(error) === "cancelled") {
              result.cancelled = true;
              break;
            }
            result.failed += 1;
            setOutcomes((prev) => ({
              ...prev,
              [item.key]: { status: "failed", message: getErrorMessage(error, t("common.error")) },
            }));
          }
        }
      } finally {
        unlisten();
        operationKey.current = null;
        setCurrent(null);
        setProgress(null);
        setRunning(false);
      }
      return result;
    },
    [t]
  );

  /** Stops after the current marketplace, cancelling its clone if one is running. */
  const cancel = useCallback(() => {
    cancelRequested.current = true;
    if (operationKey.current) {
      api.cancelInstall(operationKey.current).catch(() => {
        // The clone may have finished before the cancel arrived.
      });
    }
  }, []);

  return { running, current, progress, outcomes, run, cancel, reset };
}
