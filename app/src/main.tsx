import React from "react";
import ReactDOM from "react-dom/client";
import "./features/ui/tokens.css";
import App from "./App";
import { TAB_DRAG_TYPE } from "./features/workspace/TabStrip";
import { suppressDefaultContextMenu } from "./features/ui/contextMenu";

// OS 파일 놓기 처리를 껐으므로 앱 밖에서 끌어온 파일·링크가 문서를 이동시키지 않게 막는다.
// 탭 끌기와 입력칸 텍스트 놓기는 그대로 둔다.
for (const type of ["dragover", "drop"] as const) {
  window.addEventListener(type, (event) => {
    const types = event.dataTransfer?.types ?? [];
    if (types.includes(TAB_DRAG_TYPE) || !(types.includes("Files") || types.includes("text/uri-list"))) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "none";
  });
}

window.addEventListener("contextmenu", suppressDefaultContextMenu);

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
