import { useRef, useState } from "react";
import { TerminalView } from "./features/terminal/TerminalView";

function App() {
  const nextTab = useRef(2);
  const [tabs, setTabs] = useState([1]);
  const [active, setActive] = useState(1);

  function openTab() {
    const tab = nextTab.current++;
    setTabs([...tabs, tab]);
    setActive(tab);
  }

  function closeTab(tab: number) {
    const index = tabs.indexOf(tab);
    const remaining = tabs.filter((id) => id !== tab);
    if (active === tab) setActive(remaining[index] ?? remaining[index - 1] ?? 0);
    setTabs(remaining);
  }

  return (
    <main className="terminal-workspace">
      <nav className="terminal-tabs" aria-label="로컬 터미널">
        {tabs.map((tab) => (
          <div className="terminal-tab" key={tab}>
            <button aria-pressed={tab === active} onClick={() => setActive(tab)}>로컬 {tab}</button>
            <button aria-label={`로컬 ${tab} 닫기`} onClick={() => closeTab(tab)}>×</button>
          </div>
        ))}
        <button onClick={openTab}>+ 로컬 열기</button>
      </nav>
      {tabs.map((tab) => <TerminalView key={tab} active={tab === active} label={`로컬 ${tab}`} />)}
    </main>
  );
}

export default App;
