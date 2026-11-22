import type { ObjectRow } from "./s3";

export function ObjectTable({ rows, selectedKey, onOpenPrefix, onSelectObject }: {
  rows: readonly ObjectRow[];
  selectedKey: string | null;
  onOpenPrefix(prefix: string): void;
  onSelectObject(key: string): void;
}) {
  return <table aria-label="객체 목록" className="storage-objects">
    <thead><tr><th scope="col">유형</th><th scope="col">전체 키</th><th scope="col">크기 (바이트)</th></tr></thead>
    <tbody>{rows.map(row => row.kind === "prefix"
      ? <tr key={`prefix:${row.prefix}`}>
        <td>접두어</td><td><button type="button" onClick={() => onOpenPrefix(row.prefix)}>{row.prefix}</button></td><td>—</td>
      </tr>
      : <tr key={`object:${row.key}`}>
        <td>객체</td><td><button type="button" aria-pressed={selectedKey === row.key} onClick={() => onSelectObject(row.key)}>{row.key}</button></td>
        <td>{row.bytes ?? "—"}</td>
      </tr>)}</tbody>
  </table>;
}
