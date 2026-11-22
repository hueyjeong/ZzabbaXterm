import { invoke } from "@tauri-apps/api/core";

export type TunnelRequest = Readonly<{ listen: number } & (
  | { direction: "local"; host: string; port: number; protocol?: "rdp" | "vnc" }
  | { direction: "remote"; host: string; port: number }
  | { direction: "dynamic" }
)>;

export type TunnelStatus = Readonly<{
  id: string;
  endpoint: string;
  route: string;
  owners: readonly string[];
  state: "starting" | "ready" | "failed" | "disconnected";
  error: string | null;
}>;

export const tunnelIpc = {
  open: (target: string, label: string) => invoke<string>("tunnel_tab_open", { target, label }),
  start: (tab: string, request: TunnelRequest) => invoke<string | null>("tunnel_start", { tab, request }),
  status: (tab: string) => invoke<TunnelStatus[]>("tunnel_status", { tab }),
  share: (tab: string, to: string, id: string) => invoke<null>("tunnel_share", { tab, to, id }),
  close: (tab: string) => invoke<null>("tunnel_tab_close", { tab }),
};
