export type CompatibleAddressing = "path" | "virtual_hosted";

export type CompatibleSettingsInput = Readonly<{
  serviceId: string;
  alias: string;
  endpoint: string;
  region: string;
  addressing: CompatibleAddressing;
}>;

export type CompatibleSettingsView = Readonly<
  Omit<CompatibleSettingsInput, "endpoint"> & { origin: string }
>;

export type CompatibleOpenRequest =
  | Readonly<{ kind: "saved"; expected: CompatibleSettingsView }>
  | Readonly<{ kind: "select"; settings: CompatibleSettingsInput; itemId: string }>;

export type CompatibleSession = Readonly<{ sessionId: string; target: CompatibleSettingsView }>;

export type CompatibleSettingsError =
  | "invalid_endpoint"
  | "invalid_region"
  | "invalid_service_id"
  | "invalid_item_id"
  | "invalid_alias"
  | "invalid_file"
  | "unsupported_version"
  | "not_found"
  | "io"
  | "wrong_service"
  | "cancelled"
  | "key_missing"
  | "key_locked"
  | "key_unavailable"
  | "key_invalid";
