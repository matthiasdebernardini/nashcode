/*
 * The browser entry, bundled by esbuild into OUT_DIR/nashcode.js.
 *
 * Three jobs, all progressive enhancement over server-rendered HTML:
 *  1. render each embedded unified diff with @pierre/diffs (the real FileDiff
 *     component), attaching line-anchored comments through its annotation slots;
 *  2. native drag-and-drop on the board, POSTing moves to the server;
 *  3. small conveniences: toasts, comment-line pickers.
 */
import { FileDiff, parsePatchFiles } from "@pierre/diffs";

const THEME = { light: "github-light", dark: "github-dark" };

/* ---- diffs ------------------------------------------------------------------ */

function renderAnnotation(annotation) {
  const meta = annotation.metadata;
  if (!meta || !meta.html) return undefined;
  const el = document.createElement("div");
  el.className = "nashcode-annotation";
  el.innerHTML = meta.html;
  return el;
}

function mountDiffs() {
  for (const blob of document.querySelectorAll("script.nashcode-diff-data")) {
    let data;
    try {
      data = JSON.parse(blob.textContent);
    } catch {
      continue;
    }
    const mount = document.getElementById(data.mount);
    if (!mount || !data.patch) continue;

    try {
      const patches = parsePatchFiles(data.patch);
      const fileDiff = patches[0] && patches[0].files[0];
      if (!fileDiff) continue;
      // Click a line number to anchor the comment composer to that line.
      const composer =
        mount.closest(".Box") && mount.closest(".Box").querySelector(".nashcode-composer");
      const instance = new FileDiff({
        theme: THEME,
        themeType: "system",
        diffStyle: "unified",
        hunkSeparators: "line-info",
        renderAnnotation,
        onLineNumberClick(props) {
          if (!composer || props.annotationSide !== "additions") return;
          const line = composer.querySelector("input[name=line]");
          const body = composer.querySelector("textarea[name=body]");
          if (line) line.value = props.lineNumber;
          if (body) body.focus();
        },
      });
      // Drop the <pre> fallback once the real component takes over.
      mount.textContent = "";
      instance.render({
        fileDiff,
        containerWrapper: mount,
        lineAnnotations: data.annotations || [],
      });
    } catch (error) {
      // A diff that will not parse still has its <pre> fallback in the DOM.
      console.warn("nashcode: diff render failed for", data.file, error);
    }
  }
}

/* ---- board ------------------------------------------------------------------ */

function toast(message, tone) {
  let host = document.querySelector(".nashcode-toasts");
  if (!host) {
    host = document.createElement("div");
    host.className = "nashcode-toasts";
    document.body.appendChild(host);
  }
  const note = document.createElement("div");
  note.className = `flash ${tone === "error" ? "flash-error" : "flash-success"}`;
  note.textContent = message;
  host.appendChild(note);
  setTimeout(() => note.remove(), 6000);
}

function mountBoard() {
  const board = document.querySelector(".nashcode-board");
  if (!board) return;
  const repo = board.dataset.repo;

  for (const card of board.querySelectorAll(".nashcode-board-card")) {
    card.draggable = true;
    card.addEventListener("dragstart", (event) => {
      event.dataTransfer.setData("text/nashcode-file", card.dataset.file);
      event.dataTransfer.effectAllowed = "move";
      card.classList.add("is-dragging");
    });
    card.addEventListener("dragend", () => card.classList.remove("is-dragging"));
  }

  for (const column of board.querySelectorAll(".nashcode-board-column")) {
    if (column.dataset.nodrop === "true") continue;
    column.addEventListener("dragover", (event) => {
      if (!event.dataTransfer.types.includes("text/nashcode-file")) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "move";
      column.classList.add("is-drag-over");
    });
    column.addEventListener("dragleave", () => column.classList.remove("is-drag-over"));
    column.addEventListener("drop", async (event) => {
      event.preventDefault();
      column.classList.remove("is-drag-over");
      const file = event.dataTransfer.getData("text/nashcode-file");
      const status = column.dataset.status;
      if (!file || !status) return;

      const card = board.querySelector(`.nashcode-board-card[data-file="${CSS.escape(file)}"]`);
      const origin = card && card.parentElement;
      if (card) column.querySelector(".nashcode-board-column-body").prepend(card);

      try {
        const response = await fetch(`/${encodeURIComponent(repo)}/board/move`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ file, status }),
        });
        if (!response.ok) {
          const text = await response.text();
          throw new Error(text.slice(0, 200) || response.statusText);
        }
        toast(`Moved ${file} to ${status}`);
      } catch (error) {
        // Snap the card back where it came from.
        if (card && origin) origin.prepend(card);
        toast(`Move failed: ${error.message}`, "error");
      }
    });
  }
}

/* ---- comment line picker ----------------------------------------------------- */

// The composer's optional "line" input is plain HTML; nothing to wire yet beyond
// keeping the form usable without JS. Deliberately no framework.

function mountAll() {
  mountDiffs();
  mountBoard();
}

// A bundle this large can finish evaluating after DOMContentLoaded has already
// fired; mount immediately in that case instead of waiting for an event that
// will never come again.
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", mountAll);
} else {
  mountAll();
}

// A console escape hatch for poking at the diff renderer.
globalThis.__nashcode = { FileDiff, parsePatchFiles, mountDiffs };
