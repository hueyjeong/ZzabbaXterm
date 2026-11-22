import { useEffect, useRef, useState } from "react";

export type SsmScope = Readonly<{ profile: string; region: string }>;
export type SsmTarget = SsmScope & Readonly<{ instanceId: string }>;
export type SsmOnlineNode = Readonly<{ instanceId: string; name?: string }>;
type Failure = "SsoLoginRequired" | "AccessDenied" | "PluginMissing" | "Unknown";
export type SsmResult<T> = { readonly ok: true; readonly value: T }
  | { readonly ok: false; readonly error: Failure };

export type SsmDiscovery = Readonly<{
  listProfiles: (signal: AbortSignal) => Promise<SsmResult<readonly string[]>>;
  configuredRegion: (profile: string, signal: AbortSignal) => Promise<SsmResult<string | null>>;
  listOnlineNodes: (scope: SsmScope, signal: AbortSignal) => Promise<SsmResult<readonly SsmOnlineNode[]>>;
  relogin: (scope: SsmScope, cause: "SsoLoginRequired", signal: AbortSignal) => Promise<SsmResult<void>>;
}>;

type Profiles = { kind: "loading" }
  | { kind: "ready"; values: readonly string[] }
  | { kind: "error"; error: Failure };

type Step = { kind: "idle" }
  | { kind: "region-loading"; profile: string }
  | { kind: "region-required"; profile: string }
  | { kind: "region-error"; profile: string; error: Failure }
  | { kind: "nodes-loading"; scope: SsmScope }
  | { kind: "logging-in"; scope: SsmScope }
  | { kind: "nodes-error"; scope: SsmScope; error: Failure }
  | { kind: "nodes-ready"; scope: SsmScope; nodes: readonly SsmOnlineNode[]; selectedId: string | null };

export type SsmSelection = Readonly<{
  profiles: Profiles;
  step: Step;
  profile: string;
  region: string;
  selection: SsmTarget | null;
  chooseProfile: (profile: string) => void;
  chooseRegion: (region: string) => void;
  chooseNode: (instanceId: string) => void;
  requestRelogin: () => void;
}>;

async function resultOf<T>(request: () => Promise<SsmResult<T>>): Promise<SsmResult<T>> {
  try {
    return await request();
  } catch {
    return { ok: false, error: "Unknown" };
  }
}

export function useSsmSelection(discovery: SsmDiscovery): SsmSelection {
  const { listProfiles, configuredRegion, listOnlineNodes, relogin } = discovery;
  const [profiles, setProfiles] = useState<Profiles>({ kind: "loading" });
  const [step, setStep] = useState<Step>({ kind: "idle" });
  const login = useRef<AbortController | null>(null);

  useEffect(() => () => login.current?.abort(), []);

  useEffect(() => {
    const controller = new AbortController();
    const { signal } = controller;
    void resultOf(() => listProfiles(signal)).then((result) => {
      if (signal.aborted) return;
      setProfiles(result.ok ? { kind: "ready", values: result.value } : { kind: "error", error: result.error });
    });
    return () => controller.abort();
  }, [listProfiles]);

  useEffect(() => {
    if (step.kind !== "region-loading" && step.kind !== "nodes-loading") return;
    const controller = new AbortController();
    const { signal } = controller;
    function advance(next: Step) {
      // 조회 콜백이 취소를 무시하거나 렌더링 전에 응답해도 현재 요청만 상태를 바꾼다.
      if (!signal.aborted) setStep((current) => current === step ? next : current);
    }
    if (step.kind === "region-loading") {
      const { profile } = step;
      void resultOf(() => configuredRegion(profile, signal)).then((result) => {
        if (!result.ok) advance({ kind: "region-error", profile, error: result.error });
        else if (result.value === null) advance({ kind: "region-required", profile });
        else advance({ kind: "nodes-loading", scope: { profile, region: result.value } });
      });
    } else {
      const { scope } = step;
      void resultOf(() => listOnlineNodes(scope, signal)).then((result) => {
        advance(result.ok
          ? { kind: "nodes-ready", scope, nodes: result.value, selectedId: null }
          : { kind: "nodes-error", scope, error: result.error });
      });
    }
    return () => controller.abort();
  }, [configuredRegion, listOnlineNodes, step]);

  const scope = "scope" in step ? step.scope : null;
  const profile = scope?.profile ?? ("profile" in step ? step.profile : "");
  const region = scope?.region ?? "";
  const selection: SsmTarget | null = step.kind === "nodes-ready" && step.selectedId !== null
    ? { ...step.scope, instanceId: step.selectedId }
    : null;

  return {
    profiles, step, profile, region, selection,
    chooseProfile(value: string) {
      if (value === profile) return;
      if (value && (profiles.kind !== "ready" || !profiles.values.includes(value))) return;
      login.current?.abort();
      login.current = null;
      setStep(value ? { kind: "region-loading", profile: value } : { kind: "idle" });
    },
    chooseRegion(value: string) {
      if (!profile || value === region) return;
      login.current?.abort();
      login.current = null;
      setStep(value ? { kind: "nodes-loading", scope: { profile, region: value } } : { kind: "region-required", profile });
    },
    chooseNode(instanceId: string) {
      setStep((current) => {
        if (current !== step || current.kind !== "nodes-ready") return current;
        if (instanceId && !current.nodes.some((node) => node.instanceId === instanceId)) return current;
        return { ...current, selectedId: instanceId || null };
      });
    },
    requestRelogin() {
      if (login.current || step.kind !== "nodes-error" || step.error !== "SsoLoginRequired") return;
      const controller = new AbortController();
      const { scope } = step;
      login.current = controller;
      setStep({ kind: "logging-in", scope });
      void resultOf(() => relogin(scope, "SsoLoginRequired", controller.signal)).then((result) => {
        if (controller.signal.aborted) return;
        login.current = null;
        setStep(result.ok ? { kind: "nodes-loading", scope } : { kind: "nodes-error", scope, error: result.error });
      });
    },
  };
}
