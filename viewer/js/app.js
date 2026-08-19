/*
 * The browser entry, bundled by esbuild into OUT_DIR/nashcode.js.
 *
 * Four jobs, all progressive enhancement over server-rendered HTML:
 *  1. render each embedded unified diff with @pierre/diffs (the real FileDiff
 *     component), attaching line-anchored comments — and the composer a line click
 *     opens — through its annotation slots;
 *  2. blob pages: line anchors (always) and shiki highlighting (when the server
 *     named a language), loading one grammar chunk on demand;
 *  3. native drag-and-drop on the board, POSTing moves to the server;
 *  4. small conveniences: toasts.
 */
import { FileDiff, parsePatchFiles } from "@pierre/diffs";

const THEME = { light: "github-light", dark: "github-dark" };

/* ---- diffs ------------------------------------------------------------------ */

/*
 * Click a diff line to comment on it.
 *
 * @pierre/diffs hands us the click through `onLineClick` / `onLineNumberClick` — an
 * event API, so no DOM scraping and no chance of reading the wrong number — and takes
 * the composer back through the same annotation slots that render stored comments. The
 * composer is a clone of a server-rendered <template>: same action, same hidden fields,
 * so the server sees an ordinary form post and needs no new endpoint.
 *
 * A click that cannot name a new-side line (the deletion column) falls back to the
 * file-level composer under the diff rather than anchoring to a line it guessed.
 */

/** Closes whichever inline composer is open, so only one ever is. */
let closeOpenComposer = null;

function renderStoredAnnotation(annotation) {
  const meta = annotation.metadata;
  if (!meta || !meta.html) return undefined;
  const el = document.createElement("div");
  el.className = "nashcode-annotation";
  el.innerHTML = meta.html;
  return el;
}

function mountDiff(data) {
  const mount = document.getElementById(data.mount);
  if (!mount || !data.patch) return;
  const patches = parsePatchFiles(data.patch);
  const fileDiff = patches[0] && patches[0].files[0];
  if (!fileDiff) return;

  const box = mount.closest(".Box");
  const template = box && box.querySelector("template.nashcode-inline-composer-template");
  // The whole-file composer: where an unanchorable click lands.
  const fileComposer = box && box.querySelector(".nashcode-composer");
  const stored = data.annotations || [];

  // One metadata object, compared by identity: the renderer keeps the same composer
  // element (and whatever is typed into it) across re-renders.
  const composerMeta = { composer: true };
  let composer = null;
  let openLine = null;

  function close() {
    if (openLine === null) return;
    openLine = null;
    if (closeOpenComposer === close) closeOpenComposer = null;
    instance.render({ lineAnnotations: stored });
  }

  function buildComposer() {
    const form = template.content.firstElementChild.cloneNode(true);
    form.addEventListener("keydown", (event) => {
      if (event.key === "Escape") close();
    });
    const cancel = form.querySelector(".nashcode-inline-composer-cancel");
    if (cancel) cancel.addEventListener("click", () => close());
    return form;
  }

  function open(line) {
    if (closeOpenComposer && closeOpenComposer !== close) closeOpenComposer();
    composer = composer || buildComposer();
    const number = composer.querySelector("input[name=line]");
    if (number) number.value = String(line);
    const target = composer.querySelector(".nashcode-inline-composer-target");
    if (target) target.textContent = `Line ${line} of ${data.file}`;
    openLine = line;
    closeOpenComposer = close;
    instance.render({
      lineAnnotations: [...stored, { side: "additions", lineNumber: line, metadata: composerMeta }],
    });
    const body = composer.querySelector("textarea[name=body]");
    if (body) body.focus();
  }

  function fallBackToFile() {
    close();
    if (!fileComposer) return;
    const body = fileComposer.querySelector("textarea[name=body]");
    if (!body) return;
    body.focus();
    body.scrollIntoView({ block: "nearest" });
  }

  function onClick(props) {
    // A click that ends a text selection is a selection, not a request to comment.
    const selection = window.getSelection();
    if (selection && !selection.isCollapsed) return;
    if (!template) return;
    const line = Number(props.lineNumber);
    // Only the new side carries a line number a comment can anchor to.
    if (props.annotationSide !== "additions" || !Number.isInteger(line) || line < 1) {
      fallBackToFile();
      return;
    }
    if (openLine === line) close();
    else open(line);
  }

  const instance = new FileDiff({
    theme: THEME,
    themeType: "system",
    diffStyle: "unified",
    hunkSeparators: "line-info",
    lineHoverHighlight: "both",
    renderAnnotation(annotation) {
      if (annotation.metadata === composerMeta) return composer;
      return renderStoredAnnotation(annotation);
    },
    onLineClick: onClick,
    onLineNumberClick: onClick,
  });
  // Drop the <pre> fallback once the real component takes over.
  mount.textContent = "";
  instance.render({ fileDiff, containerWrapper: mount, lineAnnotations: stored });
}

function mountDiffs() {
  for (const blob of document.querySelectorAll("script.nashcode-diff-data")) {
    let data;
    try {
      data = JSON.parse(blob.textContent);
    } catch {
      continue;
    }
    try {
      mountDiff(data);
    } catch (error) {
      // A diff that will not parse still has its <pre> fallback in the DOM.
      console.warn("nashcode: diff render failed for", data.file, error);
    }
  }
  // Escape closes the composer from anywhere, not only from inside it.
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && closeOpenComposer) closeOpenComposer();
  });
}

/* ---- blob: line anchors and highlighting ------------------------------------- */

/*
 * Two independent enhancements over the server's numbered <pre>.
 *
 * Anchors run first and never depend on shiki: the gutter, the ids, and the hash are
 * all in the HTML already. Highlighting then swaps the *contents* of each line, so a
 * failed or skipped highlight leaves a working, linkable file behind.
 */

/**
 * `#L10` / `#L10-L20` -> [10, 20]; anything else -> null.
 *
 * `count` clamps the range to the lines that exist. A hand-typed `#L0` or `#L99999`
 * used to index past the ends of the array and throw, and one throw here would take
 * the whole blob enhancement down with it — highlighting included.
 */
function parseLineRange(hash, count) {
  const match = /^#L(\d+)(?:-L?(\d+))?$/.exec(hash || "");
  if (!match) return null;
  let start = Number(match[1]);
  let end = match[2] ? Number(match[2]) : start;
  if (start > end) [start, end] = [end, start];
  start = Math.max(1, start);
  end = Math.min(count, end);
  return start <= end ? [start, end] : null;
}

function mountBlob() {
  const pre = document.querySelector(".nashcode-blob");
  if (!pre) return;
  const lines = [...pre.querySelectorAll(".nashcode-line")];
  if (!lines.length) return;

  // Where a shift-click extends from: the last line clicked plainly, or the start of
  // the range the URL arrived with.
  let anchor = null;

  function paint(scroll) {
    const range = parseLineRange(window.location.hash, lines.length);
    for (const line of lines) line.classList.remove("is-highlighted");
    if (!range) return null;
    const [start, end] = range;
    for (let n = start; n <= end; n += 1) {
      lines[n - 1].classList.add("is-highlighted");
    }
    if (scroll) {
      lines[start - 1].scrollIntoView({ block: "center" });
    }
    return range;
  }

  function follow(scroll) {
    const range = paint(scroll);
    anchor = range ? range[0] : null;
  }

  pre.addEventListener("click", (event) => {
    const gutter = event.target.closest(".nashcode-lineno");
    if (!gutter) return;
    event.preventDefault();
    const line = Number(gutter.dataset.line);
    if (!Number.isInteger(line) || line < 1 || line > lines.length) return;
    if (event.shiftKey && anchor && anchor !== line) {
      history.replaceState(null, "", `#L${Math.min(anchor, line)}-L${Math.max(anchor, line)}`);
    } else {
      history.replaceState(null, "", `#L${line}`);
      anchor = line;
    }
    paint(false);
  });

  window.addEventListener("hashchange", () => follow(true));
  follow(true);

  highlightBlob(pre, lines).catch((error) => {
    // The plain <pre> is already correct; highlighting is the part that was optional.
    console.warn("nashcode: highlight failed", error);
  });
}

async function highlightBlob(pre, lines) {
  const lang = pre.dataset.lang;
  if (!lang) return;
  // Dynamic, so esbuild's splitting keeps shiki — and each grammar — out of the entry
  // chunk. Only the grammar this file needs is ever fetched.
  const { bundledLanguages, codeToHtml } = await import("shiki");
  if (!bundledLanguages[lang]) return;

  const bodies = lines.map((line) => line.querySelector(".nashcode-line-code"));
  const code = bodies.map((body) => (body ? body.textContent : "")).join("\n");
  const html = await codeToHtml(code, { lang, themes: THEME, defaultColor: false });

  const parsed = new DOMParser().parseFromString(html, "text/html");
  const shiki = parsed.querySelector("pre.shiki");
  if (!shiki) return;
  const highlighted = shiki.querySelectorAll("code > .line");
  // A line count that disagrees means shiki read the file differently than we split
  // it; the plain text is the one that is certainly right.
  if (highlighted.length !== bodies.length) return;

  // shiki puts every theme's colors on the root as CSS variables; the stylesheet
  // picks one by color mode.
  pre.setAttribute("style", shiki.getAttribute("style") || "");
  pre.classList.add("is-highlighted-code");
  for (let i = 0; i < bodies.length; i += 1) {
    if (bodies[i]) bodies[i].innerHTML = highlighted[i].innerHTML;
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

// Each enhancement stands alone: one that throws must not take the others with it.
// The server-rendered page underneath every one of them is already correct.
function mountAll() {
  for (const mount of [mountDiffs, mountBlob, mountBoard]) {
    try {
      mount();
    } catch (error) {
      console.warn(`nashcode: ${mount.name} failed`, error);
    }
  }
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
