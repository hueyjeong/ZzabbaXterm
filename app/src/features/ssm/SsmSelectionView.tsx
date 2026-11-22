import type { SsmSelection } from "./ssm";

const failureText = {
  SsoLoginRequired: "SSO 로그인 만료",
  AccessDenied: "IAM 권한 거부",
  PluginMissing: "Session Manager 플러그인 없음",
  Unknown: "분류되지 않은 오류",
};

export function SsmSelectionView({ state, regions }: {
  state: SsmSelection;
  regions: readonly string[];
}) {
  const { profiles, step, profile, region, selection, chooseProfile, chooseRegion, chooseNode, requestRelogin } = state;
  return (
    <fieldset>
      <legend>AWS Systems Manager</legend>
      <div>
        <label>
          프로필{" "}
          <select value={profile} disabled={profiles.kind !== "ready" || profiles.values.length === 0}
            onChange={(event) => chooseProfile(event.currentTarget.value)}>
            <option value="">프로필 선택</option>
            {profiles.kind === "ready" && profiles.values.map((value) => <option key={value} value={value}>{value}</option>)}
          </select>
        </label>
      </div>
      {profiles.kind === "loading" && <p role="status">프로필 조회 중</p>}
      {profiles.kind === "ready" && profiles.values.length === 0 && <p role="status">AWS 프로필 없음</p>}
      {profiles.kind === "error" && <p role="alert">프로필 조회: {failureText[profiles.error]}</p>}
      <div>
        <label>
          리전{" "}
          <select value={region} disabled={!profile} onChange={(event) => chooseRegion(event.currentTarget.value)}>
            <option value="">리전 선택</option>
            {region && !regions.includes(region) && <option>{region}</option>}
            {regions.map((value) => <option key={value}>{value}</option>)}
          </select>
        </label>
      </div>
      {step.kind === "region-loading" && <p role="status">기본 리전 조회 중</p>}
      {step.kind === "region-required" && <p role="status">선택된 리전 없음</p>}
      {step.kind === "region-error" && <p role="alert">기본 리전 조회: {failureText[step.error]}</p>}
      <div>
        <label>
          온라인 노드{" "}
          <select value={selection?.instanceId ?? ""} disabled={step.kind !== "nodes-ready" || step.nodes.length === 0}
            onChange={(event) => chooseNode(event.currentTarget.value)}>
            <option value="">노드 선택</option>
            {step.kind === "nodes-ready" && step.nodes.map(({ instanceId, name }) => (
              <option key={instanceId} value={instanceId}>{name ? `${name} (${instanceId})` : instanceId}</option>
            ))}
          </select>
        </label>
      </div>
      {step.kind === "nodes-loading" && <p role="status">온라인 노드 조회 중</p>}
      {step.kind === "nodes-ready" && step.nodes.length === 0 && <p role="status">온라인 노드 없음</p>}
      {step.kind === "logging-in" && <p role="status">{profile}: SSO 재로그인 중</p>}
      {step.kind === "nodes-error" && (
        <>
          <p role="alert">{profile}: {failureText[step.error]}</p>
          {step.error === "SsoLoginRequired" && <button type="button" onClick={requestRelogin}>SSO 재로그인</button>}
        </>
      )}
    </fieldset>
  );
}
