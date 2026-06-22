/**
 * ElasticsearchSection Component
 *
 * Settings section for configuring Elasticsearch sync and search.
 * Allows users to connect to an ES instance for persistent history storage
 * and enhanced full-text search with Chinese tokenization.
 */

import * as React from "react";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  ChevronDown,
  ChevronRight,
  Database,
  CheckCircle2,
  XCircle,
  Loader2,
  RefreshCw,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { api } from "@/services/api";
import { ElasticsearchStats } from "./ElasticsearchStats";
import { useAppStore } from "@/store/useAppStore";
import { invalidateEsSettingsCache } from "@/services/esSettings";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";

interface ElasticsearchSectionProps {
  isExpanded: boolean;
  onToggle: (open: boolean) => void;
}

interface SyncStatus {
  connected: boolean;
  last_full_sync: string | null;
  files_tracked: number;
  messages_count: number;
  sessions_count: number;
}

interface SyncStats {
  files_processed: number;
  messages_indexed: number;
  sessions_indexed: number;
  errors: string[];
  duration_ms: number;
}

interface SyncProgressEvent {
  phase: "starting" | "scanning" | "processing" | "flushing" | "complete" | "cancelled" | "error";
  files_processed: number;
  total_files: number;
  messages_indexed: number;
  sessions_indexed: number;
  current_file: string;
}

export function ElasticsearchSection({
  isExpanded,
  onToggle,
}: ElasticsearchSectionProps) {
  const { t } = useTranslation();
  const customClaudePaths = useAppStore(
    (s) => s.userMetadata?.settings?.customClaudePaths
  );

  // Empty defaults: the user fills these in via the settings UI; the values
  // get persisted by `loadEsSettings` / `saveEsSettings` and rehydrated on
  // mount (see the load effect below). Do not hardcode any internal endpoint
  // or credential here — this file ships in the public fork.
  const [endpoint, setEndpoint] = React.useState("");
  const [username, setUsername] = React.useState("");
  const [password, setPassword] = React.useState("");
  const [deviceId, setDeviceId] = React.useState("");

  const [connectionStatus, setConnectionStatus] = React.useState<
    "unknown" | "connected" | "failed"
  >("unknown");
  const [isTesting, setIsTesting] = React.useState(false);
  const [isSyncing, setIsSyncing] = React.useState(false);
  const [syncStatus, setSyncStatus] = React.useState<SyncStatus | null>(null);
  const [lastSyncResult, setLastSyncResult] = React.useState<SyncStats | null>(
    null
  );
  const [progress, setProgress] = React.useState<SyncProgressEvent | null>(null);

  // Subscribe to es-sync-progress events from backend (works in Tauri only).
  React.useEffect(() => {
    let unlisten: (() => void) | undefined;
    (async () => {
      try {
        const { listen } = await import("@tauri-apps/api/event");
        unlisten = await listen<SyncProgressEvent>("es-sync-progress", (e) => {
          setProgress(e.payload);
          if (e.payload.phase === "error") {
            toast.error(t("settings.es.syncErrorToast", { file: e.payload.current_file || "unknown" }));
          } else if (e.payload.phase === "complete") {
            // Refresh status when a sync (full or startup auto) completes
            void loadSyncStatus();
          }
        });
      } catch {
        // Not in Tauri (webui-server mode) — events not available
      }
    })();
    return () => {
      if (unlisten) unlisten();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Initialize device ID from hostname
  React.useEffect(() => {
    if (!deviceId) {
      setDeviceId(
        `${navigator.userAgent.includes("Mac") ? "mac" : "pc"}-${Date.now().toString(36).slice(-6)}`
      );
    }
  }, [deviceId]);

  // Load saved settings from backend on mount (works in both Tauri and webui-server).
  // Falls back to localStorage for backwards compatibility with older installs.
  React.useEffect(() => {
    (async () => {
      try {
        const saved = (await api("es_get_settings", {})) as {
          endpoint?: string;
          username?: string;
          password?: string;
          device_id?: string;
        };
        if (saved.endpoint) setEndpoint(saved.endpoint);
        if (saved.username) setUsername(saved.username);
        if (saved.password) setPassword(saved.password);
        if (saved.device_id) setDeviceId(saved.device_id);
        return;
      } catch {
        // Fall through to localStorage
      }
      try {
        const saved = localStorage.getItem("cchv-es-settings");
        if (saved) {
          const { endpoint: ep, username: u, password: p } = JSON.parse(saved);
          if (ep) setEndpoint(ep);
          if (u) setUsername(u);
          if (p) setPassword(p);
        }
      } catch { /* ignore */ }
    })();
  }, []);

  // Load status on expand
  React.useEffect(() => {
    if (isExpanded && endpoint) {
      loadSyncStatus();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isExpanded]);

  const loadSyncStatus = async () => {
    try {
      const status = (await api("es_get_sync_status", {
        endpoint,
        username: username || null,
        password: password || null,
      })) as SyncStatus;
      setSyncStatus(status);
      setConnectionStatus(status.connected ? "connected" : "failed");
    } catch {
      setConnectionStatus("unknown");
    }
  };

  const handleTestConnection = async () => {
    setIsTesting(true);
    try {
      const result = (await api("es_check_connection", {
        endpoint,
        username: username || null,
        password: password || null,
      })) as boolean;
      setConnectionStatus(result ? "connected" : "failed");
      // Persist settings to backend (cross-mode) + localStorage (legacy compat)
      if (result) {
        try {
          await api("es_save_settings", {
            endpoint,
            username: username || null,
            password: password || null,
            deviceId: deviceId || null,
          });
        } catch (err) {
          console.warn("es_save_settings failed", err);
          toast.error(t("settings.es.saveSettingsFailed"));
        }
        try {
          localStorage.setItem(
            "cchv-es-settings",
            JSON.stringify({ endpoint, username: username || null, password: password || null })
          );
        } catch { /* ignore */ }
        // Bust cache so other tabs/components reload fresh settings
        invalidateEsSettingsCache();
      }
    } catch {
      setConnectionStatus("failed");
    } finally {
      setIsTesting(false);
    }
  };

  const handleFullSync = async () => {
    setIsSyncing(true);
    setLastSyncResult(null);
    try {
      const stats = (await api("es_full_sync", {
        endpoint,
        username: username || null,
        password: password || null,
        deviceId,
        customClaudePaths: customClaudePaths?.map((p) => p.path) ?? [],
      })) as SyncStats;
      setLastSyncResult(stats);
      await loadSyncStatus();
    } catch (err) {
      toast.error(t("settings.es.syncFailedToast"));
      setLastSyncResult({
        files_processed: 0,
        messages_indexed: 0,
        sessions_indexed: 0,
        errors: [String(err)],
        duration_ms: 0,
      });
    } finally {
      setIsSyncing(false);
    }
  };

  return (
    <Collapsible open={isExpanded} onOpenChange={onToggle}>
      <CollapsibleTrigger className="flex items-center gap-2 w-full p-4 text-left hover:bg-muted/50 transition-colors">
        {isExpanded ? (
          <ChevronDown className="h-4 w-4 shrink-0" />
        ) : (
          <ChevronRight className="h-4 w-4 shrink-0" />
        )}
        <Database className="h-4 w-4 shrink-0 text-blue-500" />
        <span className="font-medium text-sm">{t("settings.es.title")}</span>
        {connectionStatus === "connected" && (
          <CheckCircle2 className="h-3.5 w-3.5 text-green-500 ml-auto" />
        )}
        {connectionStatus === "failed" && (
          <XCircle className="h-3.5 w-3.5 text-red-500 ml-auto" />
        )}
      </CollapsibleTrigger>

      <CollapsibleContent className="px-4 pb-4">
        <div className="space-y-4 pt-2">
          {/* Connection Settings */}
          <div className="space-y-3">
            <div className="space-y-1.5">
              <Label htmlFor="es-endpoint" className="text-xs">
                {t("settings.es.endpoint")}
              </Label>
              <Input
                id="es-endpoint"
                value={endpoint}
                onChange={(e) => setEndpoint(e.target.value)}
                placeholder="http://localhost:9200"
                className="h-8 text-xs"
              />
            </div>

            <div className="grid grid-cols-2 gap-2">
              <div className="space-y-1.5">
                <Label htmlFor="es-username" className="text-xs">
                  {t("settings.es.username")}
                </Label>
                <Input
                  id="es-username"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  placeholder="elastic"
                  className="h-8 text-xs"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="es-password" className="text-xs">
                  {t("settings.es.password")}
                </Label>
                <Input
                  id="es-password"
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  placeholder="..."
                  className="h-8 text-xs"
                />
              </div>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="es-device-id" className="text-xs">
                {t("settings.es.deviceId")}
              </Label>
              <Input
                id="es-device-id"
                value={deviceId}
                onChange={(e) => setDeviceId(e.target.value)}
                placeholder="my-macbook"
                className="h-8 text-xs"
              />
            </div>
          </div>

          {/* Action Buttons */}
          <div className="flex gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={handleTestConnection}
              disabled={isTesting || !endpoint}
              className="text-xs h-7"
            >
              {isTesting ? (
                <Loader2 className="h-3 w-3 animate-spin mr-1" />
              ) : null}
              {t("settings.es.testConnection")}
            </Button>

            <Button
              variant="default"
              size="sm"
              onClick={handleFullSync}
              disabled={
                isSyncing || connectionStatus !== "connected" || !deviceId
              }
              className="text-xs h-7"
            >
              {isSyncing ? (
                <Loader2 className="h-3 w-3 animate-spin mr-1" />
              ) : (
                <RefreshCw className="h-3 w-3 mr-1" />
              )}
              {isSyncing ? t("settings.es.syncing") : t("settings.es.fullSync")}
            </Button>

            {isSyncing && (
              <Button
                variant="outline"
                size="sm"
                onClick={async () => {
                  try {
                    await api("es_cancel_sync", {});
                  } catch (err) {
                    console.warn("cancel sync failed", err);
                    toast.error(t("settings.es.cancelSyncFailed"));
                  }
                }}
                className="text-xs h-7"
              >
                {t("common.cancel")}
              </Button>
            )}
          </div>

          {/* Live progress (during active sync) */}
          {progress &&
            (progress.phase === "scanning" ||
              progress.phase === "starting" ||
              progress.phase === "processing") && (
              <div className="rounded-md border border-blue-500/30 bg-blue-500/5 p-3 text-xs space-y-1.5">
                <div className="flex justify-between font-medium">
                  <span>
                    {progress.phase === "scanning"
                      ? t("settings.es.scanningFiles")
                      : progress.phase === "starting"
                        ? t("settings.es.startingSync")
                        : t("settings.es.syncInProgress")}
                  </span>
                  {progress.total_files > 0 && (
                    <span>
                      {progress.files_processed}/{progress.total_files}
                    </span>
                  )}
                </div>
                {progress.total_files > 0 && (
                  <div className="h-1.5 rounded-full bg-blue-500/20 overflow-hidden">
                    <div
                      className="h-full bg-blue-500 transition-all"
                      style={{
                        width: `${Math.min(100, Math.round((progress.files_processed / progress.total_files) * 100))}%`,
                      }}
                    />
                  </div>
                )}
                <div className="flex justify-between text-muted-foreground">
                  <span className="truncate max-w-[60%]" title={progress.current_file}>
                    {progress.current_file || "—"}
                  </span>
                  <span>
                    {t("settings.es.progressCounts", {
                      messages: progress.messages_indexed.toLocaleString(),
                      sessions: progress.sessions_indexed,
                    })}
                  </span>
                </div>
              </div>
            )}

          {/* Sync Status */}
          {syncStatus && (
            <div
              className={cn(
                "rounded-md border p-3 text-xs space-y-1",
                syncStatus.connected
                  ? "border-green-500/30 bg-green-500/5"
                  : "border-red-500/30 bg-red-500/5"
              )}
            >
              <div className="flex justify-between">
                <span className="text-muted-foreground">{t("settings.es.status")}</span>
                <span
                  className={
                    syncStatus.connected ? "text-green-600" : "text-red-600"
                  }
                >
                  {syncStatus.connected ? t("settings.es.connected") : t("settings.es.disconnected")}
                </span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">{t("settings.es.messages")}</span>
                <span>{syncStatus.messages_count.toLocaleString()}</span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">{t("settings.es.sessions")}</span>
                <span>{syncStatus.sessions_count.toLocaleString()}</span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">{t("settings.es.filesTracked")}</span>
                <span>{syncStatus.files_tracked}</span>
              </div>
              {syncStatus.last_full_sync && (
                <div className="flex justify-between">
                  <span className="text-muted-foreground">{t("settings.es.lastSync")}</span>
                  <span>
                    {new Date(syncStatus.last_full_sync).toLocaleString()}
                  </span>
                </div>
              )}
            </div>
          )}

          {/* Last Sync Result */}
          {lastSyncResult && (
            <div
              className={cn(
                "rounded-md border p-3 text-xs space-y-1",
                lastSyncResult.errors.length === 0
                  ? "border-blue-500/30 bg-blue-500/5"
                  : "border-yellow-500/30 bg-yellow-500/5"
              )}
            >
              <div className="font-medium mb-1">{t("settings.es.syncResult")}</div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">{t("settings.es.filesProcessed")}</span>
                <span>{lastSyncResult.files_processed}</span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">{t("settings.es.messagesIndexed")}</span>
                <span>
                  {lastSyncResult.messages_indexed.toLocaleString()}
                </span>
              </div>
              <div className="flex justify-between">
                <span className="text-muted-foreground">{t("settings.es.duration")}</span>
                <span>{(lastSyncResult.duration_ms / 1000).toFixed(1)}s</span>
              </div>
              {lastSyncResult.errors.length > 0 && (
                <div className="text-yellow-600 mt-1">
                  {t("settings.es.errorCount", { count: lastSyncResult.errors.length })}
                </div>
              )}
            </div>
          )}

          {/* Statistics Dashboard */}
          {connectionStatus === "connected" && (
            <div className="border-t pt-3 mt-3">
              <div className="text-xs font-medium mb-2 text-muted-foreground">{t("settings.es.statistics")}</div>
              <ElasticsearchStats
                endpoint={endpoint}
                username={username}
                password={password}
              />
            </div>
          )}
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}
