import { useRef } from "react";

export function Splitter({ edge, label, value, preferred, min, max, units, onChange }: {
  edge: "left" | "right" | "chat";
  label: string;
  value: number;
  preferred: number;
  min: number;
  max: number;
  units: number;
  onChange: (value: number) => void;
}) {
  const drag = useRef<{ pointer: number; start: number; value: number; preferred: number } | null>(null);
  const vertical = edge !== "chat";
  const direction = edge === "right" ? -1 : 1;
  return (
    <div className={`workspace-splitter splitter-${edge}`} role="separator" tabIndex={0}
      aria-label={label} aria-orientation={vertical ? "vertical" : "horizontal"}
      aria-valuenow={Math.round(value)} aria-valuemin={Math.min(min, Math.floor(value))} aria-valuemax={max}
      onPointerDown={(event) => {
        if (event.button !== 0 || drag.current) return;
        event.preventDefault();
        event.currentTarget.focus();
        event.currentTarget.setPointerCapture(event.pointerId);
        drag.current = { pointer: event.pointerId, start: vertical ? event.clientX : event.clientY, value, preferred };
      }}
      onPointerMove={(event) => {
        const start = drag.current;
        if (start?.pointer !== event.pointerId) return;
        const delta = ((vertical ? event.clientX : event.clientY) - start.start) * units * direction;
        const next = Math.round(Math.max(min, Math.min(max, start.value + delta)));
        onChange(next === start.value ? start.preferred : next);
      }}
      onLostPointerCapture={(event) => {
        if (drag.current?.pointer === event.pointerId) drag.current = null;
      }}
      onKeyDown={(event) => {
        const step = vertical ? 20 : 5;
        let next: number;
        switch (event.key) {
          case "Home": next = min; break;
          case "End": next = max; break;
          case "ArrowLeft": if (!vertical) return; next = value - step * direction; break;
          case "ArrowRight": if (!vertical) return; next = value + step * direction; break;
          case "ArrowUp": if (vertical) return; next = value - step; break;
          case "ArrowDown": if (vertical) return; next = value + step; break;
          default: return;
        }
        event.preventDefault();
        const bounded = Math.max(min, Math.min(max, next));
        if (bounded !== value) onChange(bounded);
      }}
    />
  );
}
