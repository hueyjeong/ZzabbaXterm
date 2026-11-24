import type { S3Settings } from "../profiles";
import { Field, type FieldProps } from "./Field";

type Props = FieldProps & Readonly<{ s3: S3Settings; edit(field: keyof S3Settings, value: string): void }>;

const s3Fields = [["profile", "IAM Identity Center 프로필"], ["region", "버킷 리전"],
  ["bucket", "버킷"], ["prefix", "접두어 (선택)"]] as const;

export function S3Fields({ s3, edit, error, touch }: Props) {
  return <>
    {s3Fields.map(([field, label]) => <Field key={field} label={label} error={error(`s3.${field}`)}>
      <input value={s3[field]} spellCheck={false} onChange={event => edit(field, event.currentTarget.value)}
        onBlur={() => touch(`s3.${field}`)} />
    </Field>)}
  </>;
}
