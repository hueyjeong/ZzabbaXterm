import type { KeyboardEvent } from "react";
import { ChevronRight, Database, Folder, FolderOpen, ListRestart } from "lucide-react";
import { IconButton } from "../ui/controls";
import type { BucketRow, S3Location } from "./s3";
import { folderLabel } from "./s3Path";

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

export type PrefixTree = ReadonlyMap<string, ReadonlyMap<string, readonly string[]>>;

type Item = Readonly<{
  id: string;
  label: string;
  title: string;
  level: number;
  location: S3Location;
  bucket: boolean;
  current: boolean;
  expanded?: boolean;
}>;

// 아직 열지 않은 항목은 닫힌 표시, 열어 본 빈 항목은 표시 없음.
function caret(children: readonly string[] | undefined, open: boolean) {
  if (children === undefined) return false;
  return children.length ? open : undefined;
}

/** 버킷과 이미 본 접두어를 평면 트리 항목으로 만든다. 현재 위치의 조상만 펼친다. */
function treeItems(buckets: readonly BucketRow[], current: S3Location | undefined, tree: PrefixTree): Item[] {
  const items: Item[] = [];
  for (const { name, region } of buckets) {
    const here = current?.bucket === name;
    const children = here ? tree.get(name) : undefined;
    items.push({
      id: `b:${name}`, label: name, title: `${name} · ${region}`, level: 1, bucket: true,
      location: { bucket: name, prefix: "", region: here ? current.region : region },
      current: here && current.prefix === "", expanded: caret(children?.get(""), here),
    });
    if (!here || !children) continue;
    const walk = (parent: string, level: number) => {
      for (const child of [...children.get(parent) ?? []].sort((a, b) => collator.compare(a, b))) {
        if (child.length <= parent.length || !child.startsWith(parent)) continue;
        const open = current.prefix.startsWith(child);
        items.push({
          id: `p:${child}`, label: folderLabel(child.slice(parent.length)), title: child, level, bucket: false,
          location: { ...current, prefix: child }, current: current.prefix === child,
          expanded: caret(children.get(child), open),
        });
        if (open) walk(child, level + 1);
      }
    };
    walk("", 2);
  }
  return items;
}

export function S3Tree({ buckets, current, tree, disabled, canList, loadMore, onOpen, onListBuckets, onLoadMore }: {
  buckets: readonly BucketRow[];
  current?: S3Location;
  tree: PrefixTree;
  disabled: boolean;
  canList: boolean;
  loadMore: boolean;
  onOpen(location: S3Location): void;
  onListBuckets(): void;
  onLoadMore(): void;
}) {
  const items = treeItems(buckets, current, tree);
  const focusIndex = Math.max(0, items.findIndex(item => item.current));

  function keyDown(event: KeyboardEvent<HTMLElement>, item: Item) {
    const nodes = [...event.currentTarget.closest('[role="tree"]')!.querySelectorAll<HTMLElement>('[role="treeitem"]')];
    const index = nodes.indexOf(event.currentTarget);
    let next: HTMLElement | undefined;
    switch (event.key) {
      case "ArrowDown": next = nodes[index + 1]; break;
      case "ArrowUp": next = nodes[index - 1]; break;
      case "Home": next = nodes[0]; break;
      case "End": next = nodes[nodes.length - 1]; break;
      case "Enter": case " ":
        event.preventDefault();
        if (!disabled) onOpen(item.location);
        return;
      default: return;
    }
    event.preventDefault();
    next?.focus();
  }

  return <nav className="s3-tree" aria-label="버킷과 폴더">
    <header className="s3-pane-head">
      <h3>버킷</h3>
      {canList && <IconButton small icon={ListRestart} label="버킷 목록" disabled={disabled} onClick={onListBuckets} />}
    </header>
    <div className="s3-tree-scroll">
      {items.length > 0 && <div role="tree" aria-label="버킷과 폴더">
        {items.map((item, index) => {
          const Icon = item.bucket ? Database : item.current ? FolderOpen : Folder;
          return <div key={item.id} role="treeitem" className="s3-tree-item" aria-level={item.level} aria-selected={item.current}
            aria-expanded={item.expanded} aria-label={item.label} title={item.title} aria-disabled={disabled || undefined}
            tabIndex={index === focusIndex ? 0 : -1} style={{ ["--level" as string]: item.level - 1 }}
            data-bucket={item.bucket ? "" : undefined}
            onClick={() => { if (!disabled) onOpen(item.location); }} onKeyDown={event => keyDown(event, item)}>
            <ChevronRight className="s3-tree-caret" data-state={item.expanded === undefined ? "leaf" : item.expanded ? "open" : "closed"} aria-hidden="true" />
            <Icon className="s3-tree-icon" aria-hidden="true" />
            <span className="s3-tree-label">{item.label}</span>
            {item.bucket && <span className="s3-tree-region">{item.location.region}</span>}
          </div>;
        })}
      </div>}
      {loadMore && <button type="button" className="ui-button ghost s3-tree-more" disabled={disabled} onClick={onLoadMore}>버킷 더 불러오기</button>}
      {canList && !buckets.length && <button type="button" className="ui-button ghost s3-tree-more" disabled={disabled} onClick={onListBuckets}>버킷 목록 불러오기</button>}
    </div>
  </nav>;
}
