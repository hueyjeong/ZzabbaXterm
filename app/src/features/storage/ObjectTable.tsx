import { useMemo } from "react";
import { FileBrowser, type FileEntry, type VersionedFilesStatus } from "../files/FileBrowser";
import type { ObjectRow } from "./s3";

export function ObjectTable({ rows, selectedKey, onOpenPrefix, onSelectObject, onEditObject, versionedFiles }: {
  rows: readonly ObjectRow[];
  selectedKey: string | null;
  onOpenPrefix(prefix: string): void;
  onSelectObject(key: string): void;
  onEditObject?(key: string): void;
  versionedFiles?: VersionedFilesStatus;
}) {
  const entries = useMemo(() => rows.map<FileEntry>(row => row.kind === "prefix"
    ? { kind: "folder", key: row.prefix }
    : { kind: "file", key: row.key, bytes: row.bytes }), [rows]);
  return <FileBrowser label="객체 목록" nameColumn="전체 키" entries={entries} selectedKey={selectedKey}
    onOpenFolder={onOpenPrefix} onSelectFile={onSelectObject} onEditFile={onEditObject} versionedFiles={versionedFiles} />;
}
