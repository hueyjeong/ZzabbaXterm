import { useRef, useState, type KeyboardEvent } from "react";

export function LocalTree({ onOpen }: { onOpen: () => void }) {
  const [expanded, setExpanded] = useState(true);
  const [selected, setSelected] = useState<"group" | "target">("group");
  const group = useRef<HTMLLIElement>(null);
  const target = useRef<HTMLLIElement>(null);

  function navigate(event: KeyboardEvent<HTMLLIElement>, item: "group" | "target") {
    event.stopPropagation();
    const { key } = event;
    if (item === "group") {
      if (key === "ArrowRight" && !expanded) setExpanded(true);
      else if ((key === "ArrowRight" || key === "ArrowDown" || key === "End") && expanded) target.current?.focus();
      else if (key === "ArrowLeft") setExpanded(false);
      else if (key === "Enter" || key === " ") setExpanded(!expanded);
      else return;
    } else if (key === "ArrowUp" || key === "ArrowLeft" || key === "Home") group.current?.focus();
    else if (key === "Enter") onOpen();
    else return;
    event.preventDefault();
  }

  return (
    <ul className="local-tree" role="tree" aria-label="연결">
      <li ref={group} role="treeitem" aria-labelledby="local-group" aria-expanded={expanded}
        aria-selected={selected === "group"} tabIndex={selected === "group" ? 0 : -1}
        onFocus={(event) => { if (event.target === event.currentTarget) setSelected("group"); }}
        onKeyDown={(event) => navigate(event, "group")}>
        <div id="local-group" className="tree-row" onClick={() => {
          group.current?.focus();
          setExpanded(!expanded);
        }}><span aria-hidden="true">{expanded ? "▾" : "▸"}</span> 로컬</div>
        <ul role="group" hidden={!expanded}>
          <li ref={target} role="treeitem" aria-selected={selected === "target"}
            tabIndex={selected === "target" ? 0 : -1} onFocus={() => setSelected("target")}
            onClick={() => target.current?.focus()} onDoubleClick={onOpen}
            onKeyDown={(event) => navigate(event, "target")}>
            <div className="tree-row">내 PC</div>
          </li>
        </ul>
      </li>
    </ul>
  );
}
