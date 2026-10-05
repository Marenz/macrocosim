// Topology-canvas editor: clipboard, undo / redo, copy / paste /
// cut / delete / select-all hooked into the topology selection,
// the floating right-click menu, the side-panel `Add component`
// form, and helpers around them.

import { escapeHtml, notify } from "./app.js";
import { evalQuoted } from "./eval.js";
import { errorText } from "./http.js";
import { OPERATIONAL_MODES, showComponent } from "./inspect.js";
import { makeFnFor, pasteSource } from "./paste-forms.js";
import { READ_ONLY_TITLE, readSelectedMg, structureEditable } from "./routing.js";
import { ALIGN_MODES, topology } from "./topology.js";

// Structural edits rewrite the microgrid's file, so they are only
// offered on a managed one. Every entry point — palette, context
// menu, keyboard shortcut — goes through this, so a shortcut can't
// slip past a greyed-out button.
function editable() {
  if (structureEditable()) return true;
  notify(READ_ONLY_TITLE);
  return false;
}

// Editor-style clipboard for copy / paste of node subgraphs. Holds a
// snapshot of the *structure* (categories, subtypes, edges between
// the captured nodes) — runtime state (SoC, setpoints) is not part
// of the snapshot, matching duplicate's structural-only semantics.
//
// Clipboard survives until replaced; paste can be repeated to drop
// multiple copies of the same subgraph. Cleared on hard reload (page
// refresh) since we don't persist it.
export const clipboard = (() => {
  let buf = null; // { components: [{id, category, subtype}], edges: [[from,to]] }
  return {
    get: () => buf,
    isEmpty: () => buf == null || buf.components.length === 0,
    set(snapshot) {
      buf = snapshot;
    },
  };
})();

// Undo / redo over the selected microgrid's managed file. The
// history lives on the server (one stacked generated block per
// structural edit), so there is nothing to record client-side and a
// page reload — or a second tab — walks the same history.
export const undoMgr = (() => {
  async function step(direction) {
    const mgId = readSelectedMg();
    if (mgId == null) {
      notify(`Select a microgrid to ${direction}.`);
      return;
    }
    const res = await fetch(`/api/mg/${mgId}/${direction}`, { method: "POST" });
    if (!res.ok) {
      // 409 is "nothing left on that stack" or "unmanaged file" —
      // the server's own wording says which, so pass it through.
      notify(`${cap(direction)} failed: ${await errorText(res)}`);
    }
  }
  const cap = (s) => s[0].toUpperCase() + s.slice(1);
  return {
    undo: () => step("undo"),
    redo: () => step("redo"),
  };
})();

function snapshotSelection(selectedIds) {
  const components = selectedIds
    .map((id) => topology.get(id))
    .filter(Boolean)
    .map(({ id, category, subtype, hidden, operational_mode }) => ({
      id,
      category,
      subtype,
      hidden: !!hidden,
      operational_mode,
    }));
  if (!components.length) return null;
  const selected = new Set(selectedIds);
  const edges = topology
    .connections()
    .filter(([from, to]) => selected.has(from) && selected.has(to));
  return { components, edges };
}

export function copySelection() {
  const ids = topology.selectedIds();
  if (!ids.length) {
    notify("Nothing selected to copy.");
    return false;
  }
  const snap = snapshotSelection(ids);
  if (!snap) return false;
  const unknown = snap.components.find((c) => makeFnFor(c) == null);
  if (unknown) {
    notify(`Don't know how to copy a "${unknown.category}".`);
    return false;
  }
  clipboard.set(snap);
  const n = snap.components.length;
  notify(`Copied ${n} component${n > 1 ? "s" : ""} to clipboard.`, "success");
  return true;
}

// Paste the clipboard subgraph as one eval (one undo step); the
// form comes from paste-forms.js.
export async function pasteClipboard() {
  if (!editable()) return;
  if (clipboard.isEmpty()) {
    notify("Clipboard is empty — copy something first.");
    return;
  }
  await evalQuoted(pasteSource(clipboard.get()), "Paste failed");
}

export async function deleteSelection() {
  if (!editable()) return;
  const ids = topology.selectedIds();
  if (!ids.length) {
    notify("Nothing selected to delete.");
    return;
  }
  const removes = ids.map((id) => `(remove-component ${id})`).join(" ");
  const src = `(progn ${removes})`;
  await evalQuoted(src, "Delete failed");
}

export async function cutSelection() {
  if (!editable()) return;
  if (copySelection()) await deleteSelection();
}

export function selectAllVisible() {
  const ids = topology.allIds();
  if (!ids.length) return;
  topology.select(ids);
  showComponent(topology.get(ids[0]));
}

// One context-menu entry per ALIGN_MODES row; each group starts
// with a separator. Kept separate from the Topology menu (below) so
// any other canvas can offer the same align section without drifting.
export function alignMenuItems(canvas) {
  const items = [];
  let prevGroup = null;
  for (const m of ALIGN_MODES) {
    items.push({
      label: m.menu,
      title: m.title,
      action: () =>
        m.scale
          ? canvas.scaleSelection(m.scale.axis, m.scale.factor)
          : canvas.alignSelection(m.mode),
      separator: m.group !== prevGroup,
    });
    prevGroup = m.group;
  }
  return items;
}

// Renders `items` as the floating menu at (x, y): label + optional
// shortcut per row, separators between groups, viewport-clamped.
// Hidden on outside click, Esc, or after running an action.
export function showMenuItems(menu, items, x, y) {
  menu.innerHTML = items
    .map(
      (it) =>
        `${it.separator ? '<div class="ctx-separator"></div>' : ""}
        <button class="ctx-item" data-idx="${items.indexOf(it)}"${it.title ? ` title="${escapeHtml(it.title)}"` : ""}>
          <span>${it.label}</span>${it.shortcut ? `<kbd>${it.shortcut}</kbd>` : ""}
        </button>`,
    )
    .join("");
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  menu.hidden = false;
  // Clamp to viewport — menu has a fixed width so we can compare
  // after layout settles.
  requestAnimationFrame(() => {
    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) {
      menu.style.left = `${window.innerWidth - rect.width - 4}px`;
    }
    if (rect.bottom > window.innerHeight) {
      menu.style.top = `${window.innerHeight - rect.height - 4}px`;
    }
  });
  for (const btn of menu.querySelectorAll(".ctx-item")) {
    btn.addEventListener("click", () => {
      const idx = Number(btn.dataset.idx);
      hideContextMenu();
      topology.focusCanvas();
      items[idx].action();
    });
  }
}

// Bulk operational-mode set: one progn over the whole selection, so
// a single undo step covers the batch.
// A config edit, not a runtime poke — it persists and re-derives the
// runtime knobs server-side.
async function setSelectionMode(ids, mode) {
  const sets = ids
    .map((id) => `(set-component-operational-mode ${id} '${mode})`)
    .join(" ");
  const data = await evalQuoted(`(progn ${sets})`, "Mode change failed");
  if (data.ok) {
    notify(
      `Operational mode ${mode} for ${ids.length} component${ids.length > 1 ? "s" : ""}.`,
      "success",
    );
  }
}

// The operational-mode section of the Topology canvas's right-click
// menu, factored out so every caller offers the same vocabulary.
export function modeMenuItems(ids) {
  return OPERATIONAL_MODES.map((m, i) => ({
    label: `Mode: ${m.value}`,
    title: m.hint,
    separator: i === 0,
    action: () => setSelectionMode(ids, m.value),
  }));
}

// The Topology canvas's right-click menu. Items are context-
// dependent: Copy + Cut + Delete when something's selected (plus
// the operational-mode section), Paste when nothing's selected and
// the clipboard has content, and the align section when two or more
// nodes are selected.
export function showContextMenu(x, y) {
  const menu = document.getElementById("ctx-menu");
  const sel = topology.selectedIds();
  const items = [];
  // Copy is a clipboard read, and the mode rows are runtime config —
  // both stay on an unmanaged file. Cut / Delete / Paste rewrite the
  // structure, so they only appear on a managed one.
  const structural = structureEditable();
  if (sel.length) {
    items.push({ label: "Copy", shortcut: "Ctrl/Cmd+C", action: copySelection });
    if (structural) {
      items.push({ label: "Cut", shortcut: "Ctrl/Cmd+X", action: cutSelection });
      items.push({ label: "Delete", shortcut: "Del", action: deleteSelection });
    }
    items.push(...modeMenuItems(sel));
  } else if (!clipboard.isEmpty() && structural) {
    items.push({ label: "Paste", shortcut: "Ctrl/Cmd+V", action: pasteClipboard });
  }
  if (sel.length >= 2) items.push(...alignMenuItems(topology));
  if (!items.length) return; // nothing relevant; keep menu hidden
  showMenuItems(menu, items, x, y);
}

function hideContextMenu() {
  const menu = document.getElementById("ctx-menu");
  if (menu) menu.hidden = true;
}

export function setupContextMenu() {
  // Outside-click and Esc dismiss the menu. Capture phase so the
  // click that picked the menu item runs first.
  document.addEventListener("mousedown", (e) => {
    const menu = document.getElementById("ctx-menu");
    if (!menu.hidden && !menu.contains(e.target)) hideContextMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") hideContextMenu();
  });
}

// The component palette: one button per variant, click to add. A
// button's `data-make` is the inside of the make form it evals — the
// make-* head plus any keyword args (`make-meter :hidden t`).
export function setupAddForm() {
  document.getElementById("palette").addEventListener("click", async (ev) => {
    const btn = ev.target.closest(".pal-btn");
    if (!btn) return;
    if (!editable()) return;
    btn.disabled = true;
    try {
      await evalQuoted(`(${btn.dataset.make})`, "Create failed");
    } finally {
      btn.disabled = false;
    }
  });
}

// Grey the whole Add-component palette out on an unmanaged
// microgrid. Called on every microgrid-list refresh, so it tracks an
// Adopt without the user having to reopen the panel.
export function refreshPaletteLock() {
  const palette = document.getElementById("palette");
  if (!palette) return;
  const locked = !structureEditable();
  // The reason goes on the container too: a disabled button swallows
  // pointer events, so its own title never surfaces as a tooltip.
  if (locked) palette.title = READ_ONLY_TITLE;
  else palette.removeAttribute("title");
  for (const btn of palette.querySelectorAll(".pal-btn")) {
    btn.disabled = locked;
    if (locked) btn.title = READ_ONLY_TITLE;
    else btn.removeAttribute("title");
  }
}
