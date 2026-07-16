import { useEffect } from "react";
import TopBar from "./components/TopBar";
import MediaPool from "./components/MediaPool";
import Player from "./components/Player";
import Inspector from "./components/Inspector";
import Timeline from "./components/Timeline";
import Toasts from "./components/Toasts";
import ProjectDialogs from "./components/ProjectDialogs";
import ExportDialog from "./components/ExportDialog";
import StartOverlay from "./components/StartOverlay";
import { startSessionSync } from "./lib/projectActions";
import { maybeRunBench } from "./lib/bench";
import { startExportSync } from "./state/exportStore";

function App() {
  useEffect(() => {
    startSessionSync();
    startExportSync();
    // No-op unless the process was started with CUTTY_BENCH=1 (dev perf
    // acceptance — see lib/bench.ts).
    void maybeRunBench();
  }, []);

  return (
    <div className="flex h-full flex-col">
      <TopBar />
      <div className="relative flex min-h-0 flex-1">
        <MediaPool />
        <Player />
        <Inspector />
        <StartOverlay />
      </div>
      <Timeline />
      <Toasts />
      <ProjectDialogs />
      <ExportDialog />
    </div>
  );
}

export default App;
