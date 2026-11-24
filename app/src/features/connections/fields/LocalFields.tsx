import type { LocalShell } from "../profiles";
import { Field, type FieldProps } from "./Field";

type Props = FieldProps & Readonly<{ local: Readonly<{ shell: LocalShell; startDirectory: string }>; edit(startDirectory: string): void }>;

export function LocalFields({ local, edit, error, touch }: Props) {
  return <>
    <div className="editor-field"><span>셸</span> <output>{local.shell === "powerShell" ? "PowerShell" : "Bash"}</output></div>
    <Field label="시작 디렉터리" error={error("local.startDirectory")}>
      <input value={local.startDirectory} placeholder="홈 디렉터리" onChange={event => edit(event.target.value)} onBlur={() => touch("local.startDirectory")} />
    </Field>
  </>;
}
