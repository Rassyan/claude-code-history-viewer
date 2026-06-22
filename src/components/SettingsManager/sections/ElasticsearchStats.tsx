/**
 * ElasticsearchStats Component
 *
 * Displays aggregation statistics from ES: total messages, tokens,
 * top projects, models, and message distribution over time.
 */

import * as React from "react";
import { Button } from "@/components/ui/button";
import { BarChart3, Loader2 } from "lucide-react";
import { cn } from "@/lib/utils";
import { api } from "@/services/api";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";

interface EsStatsProps {
  endpoint: string;
  username: string;
  password: string;
}

interface BucketItem {
  key: string;
  doc_count: number;
}

interface StatsData {
  total_messages: number;
  aggregations: {
    by_provider?: { buckets: BucketItem[] };
    by_project?: { buckets: BucketItem[] };
    by_model?: { buckets: BucketItem[] };
    by_role?: { buckets: BucketItem[] };
    total_tokens_in?: { value: number };
    total_tokens_out?: { value: number };
    total_cost?: { value: number };
    time_range?: { min: number; max: number };
  };
}

export function ElasticsearchStats({ endpoint, username, password }: EsStatsProps) {
  const { t } = useTranslation();
  const [stats, setStats] = React.useState<StatsData | null>(null);
  const [isLoading, setIsLoading] = React.useState(false);

  const loadStats = async () => {
    setIsLoading(true);
    try {
      const result = (await api("es_get_stats", {
        endpoint,
        username: username || null,
        password: password || null,
      })) as StatsData;
      setStats(result);
    } catch (err) {
      console.error("Failed to load ES stats:", err);
      toast.error(t("settings.es.statsLoadFailed"));
    } finally {
      setIsLoading(false);
    }
  };

  React.useEffect(() => {
    if (endpoint) {
      loadStats();
    }
    // Reload when ES credentials change so stats track the active connection.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [endpoint, username, password]);

  if (!stats && !isLoading) {
    return (
      <Button variant="ghost" size="sm" onClick={loadStats} className="text-xs h-7">
        <BarChart3 className="h-3 w-3 mr-1" />
        {t("settings.es.loadStats")}
      </Button>
    );
  }

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-xs text-muted-foreground py-2">
        <Loader2 className="h-3 w-3 animate-spin" />
        {t("settings.es.loadingStats")}
      </div>
    );
  }

  if (!stats) return null;

  const aggs = stats.aggregations;
  const totalIn = aggs.total_tokens_in?.value ?? 0;
  const totalOut = aggs.total_tokens_out?.value ?? 0;
  const totalCost = aggs.total_cost?.value ?? 0;

  const formatNumber = (n: number): string => {
    if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
    if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
    return n.toLocaleString();
  };

  return (
    <div className="space-y-3 pt-2">
      {/* Overview Cards */}
      <div className="grid grid-cols-2 gap-2">
        <StatCard label={t("settings.es.stats.messages")} value={formatNumber(stats.total_messages)} />
        <StatCard label={t("settings.es.stats.tokensIn")} value={formatNumber(totalIn)} />
        <StatCard label={t("settings.es.stats.tokensOut")} value={formatNumber(totalOut)} />
        <StatCard
          label={t("settings.es.stats.cost")}
          value={totalCost > 0 ? `$${totalCost.toFixed(2)}` : t("settings.es.stats.na")}
        />
      </div>

      {/* Top Projects */}
      {aggs.by_project && aggs.by_project.buckets.length > 0 && (
        <div className="space-y-1.5">
          <div className="text-xs font-medium text-muted-foreground">{t("settings.es.stats.topProjects")}</div>
          <div className="space-y-1">
            {aggs.by_project.buckets.slice(0, 8).map((b) => (
              <BarRow
                key={b.key}
                label={b.key.replace(/^-/, "").split("-").pop() || b.key}
                value={b.doc_count}
                max={aggs.by_project!.buckets[0]!.doc_count}
              />
            ))}
          </div>
        </div>
      )}

      {/* Models */}
      {aggs.by_model && aggs.by_model.buckets.length > 0 && (
        <div className="space-y-1.5">
          <div className="text-xs font-medium text-muted-foreground">{t("settings.es.stats.models")}</div>
          <div className="space-y-1">
            {aggs.by_model.buckets.slice(0, 5).map((b) => (
              <BarRow
                key={b.key}
                label={b.key}
                value={b.doc_count}
                max={aggs.by_model!.buckets[0]!.doc_count}
              />
            ))}
          </div>
        </div>
      )}

      {/* Providers */}
      {aggs.by_provider && aggs.by_provider.buckets.length > 0 && (
        <div className="space-y-1.5">
          <div className="text-xs font-medium text-muted-foreground">{t("settings.es.stats.providers")}</div>
          <div className="flex gap-2 flex-wrap">
            {aggs.by_provider.buckets.map((b) => (
              <span
                key={b.key}
                className="text-xs px-2 py-0.5 rounded-full bg-muted"
              >
                {b.key}: {formatNumber(b.doc_count)}
              </span>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function StatCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-md border p-2 text-center">
      <div className="text-lg font-semibold">{value}</div>
      <div className="text-[10px] text-muted-foreground">{label}</div>
    </div>
  );
}

function BarRow({
  label,
  value,
  max,
}: {
  label: string;
  value: number;
  max: number;
}) {
  const percent = max > 0 ? (value / max) * 100 : 0;
  return (
    <div className="flex items-center gap-2 text-xs">
      <span className="w-24 truncate text-muted-foreground" title={label}>
        {label}
      </span>
      <div className="flex-1 h-3 rounded-full bg-muted overflow-hidden">
        <div
          className={cn("h-full rounded-full bg-sky-500/60")}
          style={{ width: `${percent}%` }}
        />
      </div>
      <span className="w-12 text-right text-muted-foreground">
        {value.toLocaleString()}
      </span>
    </div>
  );
}
