import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { CornerDownLeft, Search } from "lucide-react";
import "./quickOpen.css";

export type QuickItem = Readonly<{
  id: string;
  section: string;
  label: string;
  detail?: string;
  icon: ReactNode;
  run: () => void;
}>;

/** 검색어의 모든 글자가 순서대로 들어 있으면 일치로 본다. 연속·앞부분 일치에 가산점을 준다. */
export function score(text: string, query: string) {
  const haystack = text.toLocaleLowerCase();
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return 1;
  const direct = haystack.indexOf(needle);
  if (direct >= 0) return 100 - direct;
  let position = 0;
  let total = 0;
  for (const character of needle) {
    if (character === " ") continue;
    const found = haystack.indexOf(character, position);
    if (found < 0) return 0;
    total += found === position ? 3 : 1;
    position = found + 1;
  }
  return total;
}

export function QuickOpen({ items, onClose }: { items: readonly QuickItem[]; onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const listId = useId();
  const restore = useRef<Element | null>(null);

  const results = useMemo(() => items
    .map(item => ({ item, score: Math.max(score(item.label, query), score(`${item.label} ${item.detail ?? ""}`, query) * 0.8) }))
    .filter(result => result.score > 0)
    .sort((a, b) => query.trim() ? b.score - a.score : 0)
    .map(result => result.item), [items, query]);
  const current = Math.min(index, Math.max(0, results.length - 1));

  useLayoutEffect(() => {
    restore.current = document.activeElement;
    input.current?.focus();
  }, []);
  useEffect(() => {
    list.current?.querySelector<HTMLElement>('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest" });
  }, [current, results]);

  function close(run?: () => void) {
    onClose();
    if (run) run();
    else if (restore.current instanceof HTMLElement) restore.current.focus();
  }

  let section = "";
  return <div className="quick-open-backdrop" onMouseDown={event => { if (event.target === event.currentTarget) close(); }}>
    <div className="quick-open" role="dialog" aria-modal="true" aria-label="빠른 열기">
      <div className="quick-open-input">
        <Search aria-hidden="true" />
        <input ref={input} className="quick-open-field" role="combobox" aria-expanded="true" aria-controls={listId}
          aria-activedescendant={results[current] ? `${listId}-${results[current].id}` : undefined}
          aria-label="연결·명령·탭 검색" placeholder="연결, 명령, 탭 검색" value={query}
          onChange={event => { setQuery(event.target.value); setIndex(0); }}
          onKeyDown={event => {
            if (event.nativeEvent.isComposing) return;
            switch (event.key) {
              case "ArrowDown": setIndex((current + 1) % Math.max(1, results.length)); break;
              case "ArrowUp": setIndex((current - 1 + results.length) % Math.max(1, results.length)); break;
              case "Enter": if (results[current]) close(results[current].run); break;
              case "Escape": close(); break;
              default: return;
            }
            event.preventDefault();
          }} />
      </div>
      <ul ref={list} id={listId} className="quick-open-list" role="listbox" aria-label="검색 결과">
        {results.map((item, position) => {
          const header = item.section !== section ? (section = item.section) : undefined;
          return <li key={item.id} role="presentation">
            {header && <div className="quick-open-section" role="presentation">{header}</div>}
            <div id={`${listId}-${item.id}`} role="option" aria-selected={position === current} className="quick-open-item"
              onMouseMove={() => { if (position !== current) setIndex(position); }}
              onMouseDown={event => event.preventDefault()} onClick={() => close(item.run)}>
              {item.icon}
              <span className="quick-open-label">{item.label}</span>
              {item.detail && <span className="quick-open-detail">{item.detail}</span>}
              {position === current && <CornerDownLeft className="quick-open-enter" aria-hidden="true" />}
            </div>
          </li>;
        })}
        {!results.length && <li className="quick-open-empty" role="presentation">결과 없음</li>}
      </ul>
    </div>
  </div>;
}
