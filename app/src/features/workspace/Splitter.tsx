import { useRef } from "react";

export function Splitter({ edge, label, controls, value, preferred = value, min, max, onChange }: {
  edge: "left" | "right" | "center";
  label: string;
  controls?: string;
  value: number;
  preferred?: number;
  min: number;
  max: number;
  onChange: (value: number) => void;
}) {
  const drag = useRef<{ pointer: number; start: number; value: number; preferred: number; last: number; scale: number } | null>(null);
  const direction = edge === "right" ? -1 : 1;
  return (
    <div className={`workspace-splitter splitter-${edge}`} role="separator" tabIndex={0}
      aria-label={label} aria-controls={controls} aria-orientation="vertical"
      aria-valuenow={Math.round(value)} aria-valuemin={min} aria-valuemax={max}
      aria-valuetext={edge === "center" ? `${Math.round(value)}%` : undefined}
      onPointerDown={(event) => {
        if (event.button !== 0 || drag.current) return;
        event.preventDefault();
        event.currentTarget.focus();
        event.currentTarget.setPointerCapture(event.pointerId);
        const part = event.currentTarget;
        const scale = edge === "center" ? 100 / Math.max(1, (part.parentElement?.clientWidth ?? 0) - part.offsetWidth) : 1;
        drag.current = { pointer: event.pointerId, start: event.clientX, value, preferred, last: preferred, scale };
      }}
      onPointerMove={(event) => {
        const start = drag.current;
        if (start?.pointer !== event.pointerId) return;
        const delta = (event.clientX - start.start) * start.scale * direction;
        const bounded = Math.round(Math.max(min, Math.min(max, start.value + delta)));
        if (delta !== 0 && bounded === start.value && start.last === start.preferred) return;
        const next = delta === 0 ? start.preferred : bounded;
        if (next !== start.last) {
          start.last = next;
          onChange(next);
        }
      }}
      onLostPointerCapture={(event) => {
        if (drag.current?.pointer === event.pointerId) drag.current = null;
      }}
      onKeyDown={(event) => {
        const step = edge === "center" ? 5 : 20;
        let next: number;
        switch (event.key) {
          case "Home": next = min; break;
          case "End": next = max; break;
          case "ArrowLeft": next = value - step * direction; break;
          case "ArrowRight": next = value + step * direction; break;
          default: return;
        }
        event.preventDefault();
        const bounded = Math.max(min, Math.min(max, next));
        if (bounded !== value) onChange(bounded);
      }}
    />
  );
}
