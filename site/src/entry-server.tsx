import { StrictMode } from "react";
import { renderToString } from "react-dom/server";
import App from "./App";
import Changelog from "./changelog/Changelog";

/**
 * Server entry used only at build time by scripts/prerender.mjs.
 *
 * Returns the static HTML for a route so the crawler-visible markup is no
 * longer an empty <div id="root">. Components are SSR-safe: locale detection
 * returns "en" when `window` is undefined and every browser API call lives in
 * a useEffect/handler, so renderToString never touches the DOM.
 */
export function render(route: string): string {
  const Component = route === "/changelog/" ? Changelog : App;
  return renderToString(
    <StrictMode>
      <Component />
    </StrictMode>,
  );
}
