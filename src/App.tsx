import { useEffect } from "react";
import TopBar from "./components/TopBar";
import MediaPool from "./components/MediaPool";
import Player from "./components/Player";
import Inspector from "./components/Inspector";
import Timeline from "./components/Timeline";
import Toasts from "./components/Toasts";
import ProjectDialogs from "./components/ProjectDialogs";
import StartOverlay from "./components/StartOverlay";
import { startSessionSync } from "./lib/projectActions";

function App() {
  useEffect(() => {
    startSessionSync();
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
    </div>
  );
}

export default App;
