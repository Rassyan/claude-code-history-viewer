/**
 * ES Settings helper — single source of truth for reading ES connection config.
 *
 * Priority:
 *   1. Backend (Tauri command `es_get_settings`) — works in both Tauri and
 *      webui-server modes.
 *   2. `localStorage` — backwards-compatible fallback for older installs.
 *
 * Cached in-memory after the first successful read to avoid round-trips.
 */

import { api } from "@/services/api";

export interface EsSettings {
  endpoint: string;
  username: string | null;
  password: string | null;
  deviceId: string | null;
}

let cached: EsSettings | null = null;
let inflight: Promise<EsSettings | null> | null = null;

/** Force a re-read on next call (e.g. after the user updates settings). */
export function invalidateEsSettingsCache(): void {
  cached = null;
  inflight = null;
}

/**
 * Get ES connection settings.
 * Returns `null` when ES has not been configured yet.
 */
export async function getEsSettings(): Promise<EsSettings | null> {
  if (cached) return cached;
  if (inflight) return inflight;

  inflight = (async () => {
    // Try backend first (works in both Tauri and webui-server)
    try {
      const fromBackend = (await api("es_get_settings", {})) as {
        endpoint?: string;
        username?: string;
        password?: string;
        device_id?: string;
      };
      if (fromBackend?.endpoint) {
        cached = {
          endpoint: fromBackend.endpoint,
          username: fromBackend.username ?? null,
          password: fromBackend.password ?? null,
          deviceId: fromBackend.device_id ?? null,
        };
        return cached;
      }
    } catch {
      // Backend unavailable (e.g., command not registered yet) — fall through
    }

    // Legacy: localStorage fallback (browser-only)
    try {
      const raw = typeof localStorage !== "undefined"
        ? localStorage.getItem("cchv-es-settings")
        : null;
      if (raw) {
        const parsed = JSON.parse(raw) as Partial<EsSettings>;
        if (parsed?.endpoint) {
          cached = {
            endpoint: parsed.endpoint,
            username: parsed.username ?? null,
            password: parsed.password ?? null,
            deviceId: parsed.deviceId ?? null,
          };
          return cached;
        }
      }
    } catch {
      // ignore
    }

    return null;
  })().finally(() => {
    inflight = null;
  });

  return inflight;
}
