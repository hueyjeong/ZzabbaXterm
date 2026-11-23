import React from "react";
import ReactDOM from "react-dom/client";
import "./features/ui/tokens.css";
import App from "./App";
import { TAB_DRAG_TYPE } from "./features/workspace/TabStrip";

// OS 파일 놓기 처리를 껐으므로 앱 밖에서 끌어온 항목이 문서를 그 파일로 이동시키지 않게 막는다. 탭 끌기는 그대로 둔다.
for (const type of ["dragover", "drop"] as const) {
  window.addEventListener(type, (event) => {
    if (!event.dataTransfer?.types.includes(TAB_DRAG_TYPE)) event.preventDefault();
  });
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
