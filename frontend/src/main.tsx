import React from "react";
import { createRoot } from "react-dom/client";

// Bundled offline fonts (no network at runtime).
import "@fontsource/space-grotesk/400.css";
import "@fontsource/space-grotesk/500.css";
import "@fontsource/space-grotesk/700.css";
import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-sans/500.css";
import "@fontsource/ibm-plex-sans/600.css";
import "@fontsource/ibm-plex-mono/400.css";
import "@fontsource/ibm-plex-mono/500.css";

import { App } from "./App";
import { Overlay } from "./Overlay";

// The same bundle drives two windows: the full interface ("main") and the tray
// quick-panel, which loads at index.html#overlay.
const isOverlay = window.location.hash === "#overlay";

createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isOverlay ? <Overlay /> : <App />}</React.StrictMode>,
);
