import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./styles.css";

type TransformResult = {
  id: string;
  raw: string;
  converted: string;
  refined: string;
  final_text: string;
  confidence: number;
  timings_ms: {
    normalize: number;
    convert_refine: number;
    full_roundtrip: number;
  };
};

type OutputMode = "raw" | "converted" | "refined";

const app = document.querySelector<HTMLDivElement>("#app")!;

app.innerHTML = `
  <main class="shell">
    <header class="topbar">
      <div>
        <h1>Romaji Agent</h1>
        <p id="status">Ready</p>
      </div>
      <button id="selection">Selection</button>
    </header>

    <section class="panes">
      <label class="pane input-pane">
        <span>Input</span>
        <textarea id="raw" spellcheck="false" autofocus placeholder="kyou mtg de hanasita todo"></textarea>
      </label>

      <button id="convert">Convert</button>

      <div class="pane result" data-mode="converted">
        <div class="pane-head">
          <span>Converted</span>
          <button id="pick-converted">Use</button>
        </div>
        <output id="converted"></output>
      </div>

      <div class="pane result selected" data-mode="refined">
        <div class="pane-head">
          <span>Refined</span>
          <button id="pick-refined">Use</button>
        </div>
        <output id="refined"></output>
      </div>
    </section>

    <footer>
      <span id="mode">refined</span>
      <span id="timing"></span>
    </footer>
  </main>
`;

const raw = document.querySelector<HTMLTextAreaElement>("#raw")!;
const converted = document.querySelector<HTMLOutputElement>("#converted")!;
const refined = document.querySelector<HTMLOutputElement>("#refined")!;
const statusText = document.querySelector<HTMLParagraphElement>("#status")!;
const timing = document.querySelector<HTMLSpanElement>("#timing")!;
const modeText = document.querySelector<HTMLSpanElement>("#mode")!;
const convertButton = document.querySelector<HTMLButtonElement>("#convert")!;
const selectionButton = document.querySelector<HTMLButtonElement>("#selection")!;

let result: TransformResult | null = null;
let mode: OutputMode = "refined";
let converting = false;

function setStatus(value: string) {
  statusText.textContent = value;
}

function setMode(next: OutputMode) {
  mode = next;
  modeText.textContent = next;
  document.querySelectorAll(".result").forEach((node) => {
    node.classList.toggle("selected", (node as HTMLElement).dataset.mode === next);
  });
}

function selectedText() {
  if (!result) return raw.value;
  if (mode === "raw") return result.raw;
  if (mode === "converted") return result.converted;
  return result.refined;
}

async function convertFromInput() {
  const input = raw.value.trim();
  if (!input || converting) return;

  converting = true;
  convertButton.disabled = true;
  setStatus("Converting");
  try {
    result = await invoke<TransformResult>("transform_text", { raw: input });
    converted.textContent = result.converted;
    refined.textContent = result.refined;
    timing.textContent = `${result.timings_ms.full_roundtrip}ms`;
    setStatus(`confidence ${result.confidence.toFixed(2)}`);
    setMode("refined");
  } catch (error) {
    setStatus(String(error));
  } finally {
    converting = false;
    convertButton.disabled = false;
  }
}

async function accept(paste: boolean) {
  if (!result) {
    await convertFromInput();
  }
  if (!result) return;

  const finalText = selectedText();
  setStatus("Accepting");
  try {
    await invoke("accept_transform", {
      result,
      finalText,
      paste,
    });
    setStatus(paste ? "Inserted" : "Copied");
  } catch (error) {
    setStatus(`Copied, paste failed: ${String(error)}`);
  }
}

async function transformSelection() {
  setStatus("Reading clipboard");
  try {
    result = await invoke<TransformResult>("transform_clipboard_selection");
    raw.value = result.raw;
    converted.textContent = result.converted;
    refined.textContent = result.refined;
    timing.textContent = `${result.timings_ms.full_roundtrip}ms`;
    setMode("refined");
    setStatus("Selection preview");
  } catch (error) {
    setStatus(String(error));
  }
}

convertButton.addEventListener("click", convertFromInput);
selectionButton.addEventListener("click", transformSelection);
document.querySelector("#pick-converted")?.addEventListener("click", () => setMode("converted"));
document.querySelector("#pick-refined")?.addEventListener("click", () => setMode("refined"));

raw.addEventListener("input", () => {
  result = null;
  converted.textContent = "";
  refined.textContent = "";
  timing.textContent = "";
  setStatus("Ready");
});

window.addEventListener("keydown", async (event) => {
  if (event.key === "Escape") {
    window.close();
    return;
  }
  if (event.key === "Tab") {
    event.preventDefault();
    const order: OutputMode[] = ["raw", "converted", "refined"];
    setMode(order[(order.indexOf(mode) + 1) % order.length]);
    return;
  }
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    await accept(true);
  }
});

listen("romaji-shortcut", () => {
  raw.focus();
  raw.select();
  setStatus("Ready");
});

invoke("app_paths").catch((error) => setStatus(String(error)));
