import "@astryxdesign/core/reset.css";
import "@astryxdesign/core/astryx.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Theme } from "@astryxdesign/core/theme";
import { defaultThemeSlug, themes } from "./astryx-themes";
import { App } from "./App";

const rootEl = document.getElementById("root");
if (rootEl) {
  createRoot(rootEl).render(
    <StrictMode>
      <Theme theme={themes[defaultThemeSlug]} mode="dark">
        <App />
      </Theme>
    </StrictMode>
  );
}
