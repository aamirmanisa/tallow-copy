import "./styles.css";
import { jobStore } from "./state/jobStore";
import { createApp } from "./ui/App";

const app = document.querySelector<HTMLElement>("#app");

if (!app) {
  throw new Error("Missing #app root element");
}

app.replaceChildren(createApp());

// Bring back what this install has already done; the console renders the history view from it.
void jobStore.hydrateHistory();
