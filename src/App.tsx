import TopBar from "./components/TopBar";
import MediaPool from "./components/MediaPool";
import Player from "./components/Player";
import Inspector from "./components/Inspector";
import Timeline from "./components/Timeline";

function App() {
  return (
    <div className="flex h-full flex-col">
      <TopBar />
      <div className="flex min-h-0 flex-1">
        <MediaPool />
        <Player />
        <Inspector />
      </div>
      <Timeline />
    </div>
  );
}

export default App;
