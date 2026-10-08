import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { DesktopUpdateStatus } from "./types";

export const RELEASES_URL = "https://github.com/krazyjakee/3DAM/releases";
const UPDATE_KEY = ["desktop-updates"] as const;

export function hasDesktopUpdater(): boolean {
  return window.__3DAM_EMBEDDED_SERVER__ === true && !!window.__TAURI__?.core?.invoke;
}

function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const core = window.__TAURI__?.core;
  if (!hasDesktopUpdater() || !core) return Promise.reject(new Error("Open the local desktop app to use updates."));
  return core.invoke<T>(command, args);
}

export function updateBusy(status?: DesktopUpdateStatus): boolean {
  return !!status && ["checking", "downloading", "installing"].includes(status.phase);
}

export function useDesktopUpdates() {
  return useQuery({
    queryKey: UPDATE_KEY,
    queryFn: () => invoke<DesktopUpdateStatus>("desktop_update_status"),
    enabled: hasDesktopUpdater(),
    retry: false,
    refetchInterval: (query) => updateBusy(query.state.data) ? 500 : 15_000,
    refetchIntervalInBackground: true,
  });
}

export function useUpdateAction(command: "desktop_update_check" | "desktop_update_install" | "desktop_update_preferences") {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args?: Record<string, unknown>) => invoke<DesktopUpdateStatus>(command, args),
    onSuccess: (status) => client.setQueryData(UPDATE_KEY, status),
  });
}

export function restartAfterUpdate(): Promise<void> {
  return invoke("desktop_update_restart");
}
