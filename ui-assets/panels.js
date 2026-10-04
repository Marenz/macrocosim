// Microgrids landing-page list and Scenarios mode panel. Both
// poll the corresponding /api endpoint, render a card grid, and
// respond to clicks / WS pushes by re-fetching + re-rendering.

import { escapeHtml, mutate, notify, selectMicrogrid } from "./app.js";
import { refreshPaletteLock } from "./editor.js";
import { publishMgFlags, readSelectedMg, reconcileSelection, renderReplMgChip } from "./routing.js";

// Lowest id `/api/microgrids/create` allocates when none is asked
// for (`DEFAULT_MICROGRID_ID` server-side). The create dialog
// pre-fills the same choice the server would make, computed off the
// list the panel already holds, so the field shows a real id rather
// than a blank the user has to guess at.
const FIRST_MICROGRID_ID = 2200;

const UNMANAGED_HINT =
  "hand-written file — macrocosim won't rewrite its structure until you Adopt it";

export const microgridsPanel = (() => {
  let cached = []; // last /api/microgrids snapshot
  let pollTimer = null;

  function gridEl() { return document.getElementById("mglist-grid"); }
  // Warn when a microgrid was created but its gRPC server failed.
  function warnFailedRuntime(id, runtime) {
    if (runtime?.status === "failed") {
      notify(`Microgrid #${id} was created, but its gRPC server failed: ${runtime.error}`);
    }
  }
  function breadcrumbNameEl() { return document.getElementById("mg-breadcrumb-name"); }
  function breadcrumbTsoEl() { return document.getElementById("mg-breadcrumb-tso"); }

  function renderList() {
    const grid = gridEl();
    if (!grid) return;
    grid.innerHTML = "";
    for (const m of cached) {
      const card = document.createElement("button");
      card.type = "button";
      card.className = "mglist-card";
      card.dataset.id = m.id;
      const tso = m.tso ? `<span class="mg-tso">${escapeHtml(m.tso)}</span>` : "";
      // Two file-state chips: `unmanaged` means macrocosim may not
      // rewrite this file's structure (Adopt first), `unsaved` means
      // an edit ran live that the file could not be given.
      const runtime = m.runtime;
      const chips = [
        m.managed
          ? ""
          : `<span class="mg-chip unmanaged" title="${escapeHtml(UNMANAGED_HINT)}">unmanaged</span>`,
        m.unsaved
          ? `<span class="mg-chip unsaved" title="live edits this file could not record">unsaved</span>`
          : "",
        runtime?.status === "failed"
          ? `<span class="mg-chip failed" title="${escapeHtml(runtime.error ?? "")}">failed</span>`
          : "",
      ].join("");
      // A starting runtime shows "starting…", a started one its
      // bound address or "not bound"; the configured port stands in
      // when no runtime was started.
      let grpc = `:${m.grpc_port}`;
      if (runtime?.status === "starting") grpc = "starting…";
      else if (runtime) grpc = runtime.grpc_addr ?? "not bound";
      card.innerHTML = `
        <span class="mglist-id">#${m.id}</span>
        <h3 class="mglist-name">${escapeHtml(m.name || "(unnamed)")}</h3>
        ${tso}${chips}
        <span class="mglist-meta muted">${m.component_count} components · gRPC ${escapeHtml(grpc)}</span>
      `;
      card.addEventListener("click", () => selectMicrogrid(m.id));
      grid.appendChild(card);
    }
    // Trailing [+ New microgrid] card: opens the create dialog,
    // which POSTs /api/microgrids/create and selects the new entry.
    const newCard = document.createElement("button");
    newCard.type = "button";
    newCard.className = "mglist-card mglist-new";
    newCard.id = "mglist-new-btn";
    newCard.innerHTML = `<span class="mglist-plus">+</span><span>New microgrid</span>`;
    newCard.addEventListener("click", () => showCreateMgDialog());
    grid.appendChild(newCard);
    // Trailing [⇪ Import site…] card: picks a site export
    // (components.json + connections.json together) and POSTs
    // /api/microgrids/import, which creates a real simulated
    // microgrid with the export's capacity / SoC / rated bounds.
    const importCard = document.createElement("button");
    importCard.type = "button";
    importCard.className = "mglist-card mglist-new";
    importCard.id = "mglist-import-btn";
    importCard.title =
      "Load a microgrid API site export: pick components.json and connections.json together (connections optional)";
    importCard.innerHTML = `<span class="mglist-plus">⇪</span><span>Import site…</span>`;
    importCard.addEventListener("click", () => {
      document.getElementById("import-files").click();
    });
    grid.appendChild(importCard);
    // Trailing [▶ Load script…] card: opens the server-side file
    // browser dialog, which POSTs /api/load so an on-disk lisp file
    // (a microgrid file, or a script like examples/berlin-demo.lisp)
    // builds its world at runtime — the on-demand path for a bare
    // boot.
    const loadCard = document.createElement("button");
    loadCard.type = "button";
    loadCard.className = "mglist-card mglist-new";
    loadCard.id = "mglist-load-btn";
    loadCard.title =
      "Browse the server's state dir for a lisp script to load, or type a path";
    loadCard.innerHTML = `<span class="mglist-plus">▶</span><span>Load script…</span>`;
    loadCard.addEventListener("click", () => showLoadScriptDialog());
    grid.appendChild(loadCard);
  }

  // POST /api/load for a state-dir-relative (or absolute) path.
  // Returns true when the file loaded. The one recoverable failure
  // is a 409 collision — the file declares an id something else
  // already loaded — which renders the offer bar instead of a toast.
  async function loadScript(path) {
    collisionBar().hidden = true;
    let res;
    try {
      res = await fetch("/api/load", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ path }),
      });
    } catch (e) {
      notify(`Load failed: ${e.message}`);
      return false;
    }
    if (res.ok) {
      // A file is allowed to register nothing — a driver-only script
      // is exactly that — but from the Load picker it looks like a
      // no-op, so say what happened rather than leaving a silently
      // unchanged grid.
      let loaded = null;
      try {
        loaded = (await res.json()).loaded;
      } catch (_) {}
      if (Array.isArray(loaded) && loaded.length === 0) {
        notify("Loaded no microgrids — the file ran but registered nothing");
      }
      await refresh();
      return true;
    }
    // The collision 409's body is JSON served as plain text, so it
    // has to be parsed by hand; anything else (a lisp error, a
    // missing file) is already a human-readable message.
    const text = await res.text();
    if (res.status === 409) {
      let info = null;
      try {
        info = JSON.parse(text);
      } catch (_) {}
      if (info && info.collision_id != null) {
        renderCollision(path, info);
        return false;
      }
    }
    notify(`Load failed: ${text || `HTTP ${res.status}`}`);
    return false;
  }

  const collisionBar = () => document.getElementById("load-script-collision");

  // The collision offer. A managed file can be copied under a free
  // id mechanically (/api/load-as); a hand-written one can't — its
  // id is wherever the author wrote it, so the only fix is editing
  // the file.
  function renderCollision(path, info) {
    const bar = collisionBar();
    bar.innerHTML = "";
    bar.hidden = false;
    const msg = document.createElement("span");
    msg.textContent = info.managed
      ? `microgrid ${info.collision_id} is already loaded — `
      : `microgrid ${info.collision_id} is already loaded — edit the id in the file to load it alongside.`;
    bar.appendChild(msg);
    if (!info.managed) return;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "hdr-btn primary";
    btn.textContent = `Load as ${info.suggested_id}`;
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      let resp = null;
      try {
        const res = await mutate("POST", "/api/load-as", {
          path,
          id: info.suggested_id,
        });
        resp = await res.json();
      } catch (e) {
        notify(`Load failed: ${e.message}`);
        btn.disabled = false;
        return;
      }
      // The copy can load and still be half-built: its generated
      // block registered, its script section then failed. That comes
      // back as a success carrying a warning, because the microgrid
      // IS live — say so rather than dropping it.
      if (resp?.warning) notify(resp.warning);
      warnFailedRuntime(resp?.id ?? info.suggested_id, resp?.runtime);
      bar.hidden = true;
      await refresh();
      document.getElementById("load-script-dialog").close();
      selectMicrogrid(info.suggested_id);
    });
    bar.appendChild(btn);
  }

  // Server-side file browser over GET /api/scripts. The browser's
  // native picker can't drive this: it selects a client-side file
  // and never reveals a server path, while the load needs the file
  // on the server's disk (that's what reload replays and watch-file
  // can watch). The free-text field covers paths outside the
  // state-dir subtree the listing is confined to.
  function showLoadScriptDialog() {
    const dlg = document.getElementById("load-script-dialog");
    const list = document.getElementById("load-script-list");
    const crumb = document.getElementById("load-script-breadcrumb");

    async function fetchListing(dir) {
      const res = await fetch(`/api/scripts?dir=${encodeURIComponent(dir)}`);
      if (!res.ok) throw new Error(await res.text());
      return await res.json();
    }

    function renderListing(data) {
      crumb.textContent = `state dir${data.dir ? ` / ${data.dir}` : ""}`;
      list.innerHTML = "";
      const addRow = (label, onClick) => {
        const li = document.createElement("li");
        const btn = document.createElement("button");
        btn.type = "button";
        btn.textContent = label;
        btn.addEventListener("click", onClick);
        li.appendChild(btn);
        list.appendChild(li);
      };
      if (data.parent !== null) {
        addRow("📁 ..", () => browse(data.parent));
      }
      for (const d of data.dirs) {
        addRow(`📁 ${d}`, () => browse(data.dir ? `${data.dir}/${d}` : d));
      }
      for (const f of data.files) {
        const rel = data.dir ? `${data.dir}/${f}` : f;
        addRow(f, async () => {
          if (await loadScript(rel)) dlg.close();
        });
      }
      if (!data.dirs.length && !data.files.length) {
        list.innerHTML = '<li class="hint">no .lisp files here</li>';
      }
    }

    async function browse(dir) {
      try {
        renderListing(await fetchListing(dir));
      } catch (e) {
        notify(`Listing failed: ${e.message}`);
      }
    }

    // Open on microgrids/ — where managed files live, so the common
    // case is one click. A state dir without that directory answers
    // 4xx; fall back to the root listing.
    async function browseDefault() {
      try {
        renderListing(await fetchListing("microgrids"));
      } catch (_) {
        await browse("");
      }
    }

    collisionBar().hidden = true;
    browseDefault();
    dlg.showModal();
  }

  function setupLoadScriptDialog() {
    const dlg = document.getElementById("load-script-dialog");
    if (!dlg) return;
    document
      .getElementById("load-script-close")
      .addEventListener("click", () => dlg.close());
    dlg.addEventListener("click", (e) => {
      if (e.target === dlg) dlg.close();
    });
    document
      .getElementById("load-script-form")
      .addEventListener("submit", async (e) => {
        e.preventDefault();
        const input = document.getElementById("load-script-path");
        const path = input.value.trim();
        if (!path) return;
        if (await loadScript(path)) {
          input.value = "";
          dlg.close();
        }
      });
  }

  // The lowest free microgrid id — the same choice the server makes
  // for a create that names none. Walks the ascending taken ids from
  // FIRST_MICROGRID_ID, mirroring `next_free_id_in` server-side.
  function nextFreeId() {
    const taken = cached.map((m) => m.id).sort((a, b) => a - b);
    let candidate = FIRST_MICROGRID_ID;
    for (const id of taken) {
      if (id === candidate) candidate += 1;
      else if (id > candidate) break;
    }
    return candidate;
  }

  function showCreateMgDialog() {
    const dlg = document.getElementById("create-mg-dialog");
    if (!dlg) return;
    const err = document.getElementById("create-mg-error");
    err.hidden = true;
    err.textContent = "";
    document.getElementById("create-mg-name").value = "";
    // Pre-filled, not fixed: the server re-checks the id under the
    // create lock, and a taken one comes back as the inline 409.
    document.getElementById("create-mg-id").value = String(nextFreeId());
    document.getElementById("create-mg-port").value = "";
    dlg.showModal();
  }

  function setupCreateMgDialog() {
    const dlg = document.getElementById("create-mg-dialog");
    if (!dlg) return;
    document
      .getElementById("create-mg-close")
      .addEventListener("click", () => dlg.close());
    dlg.addEventListener("click", (e) => {
      if (e.target === dlg) dlg.close();
    });
    document
      .getElementById("create-mg-form")
      .addEventListener("submit", async (e) => {
        e.preventDefault();
        const err = document.getElementById("create-mg-error");
        const name = document.getElementById("create-mg-name").value.trim();
        if (!name) return;
        const body = { name };
        // A blank number field means "let the server allocate" — an
        // explicit 0 would be a claim on a real (and invalid) id.
        const id = document.getElementById("create-mg-id").value.trim();
        if (id !== "") body.id = Number(id);
        const port = document.getElementById("create-mg-port").value.trim();
        if (port !== "") body.grpc_port = Number(port);
        let created;
        try {
          created = await (await mutate("POST", "/api/microgrids/create", body)).json();
        } catch (ex) {
          // A taken id or port comes back 409 with the server's own
          // wording — shown in the dialog, which stays open so the
          // user can pick another without retyping the name.
          err.textContent = ex.message;
          err.hidden = false;
          return;
        }
        dlg.close();
        warnFailedRuntime(created.id, created.runtime);
        await refresh();
        selectMicrogrid(created.id);
      });
  }

  // The import dialog's answer, handed back to importSiteFiles. The
  // submit handler parks it here just before closing the dialog, so
  // the single `close` listener settles every way out — Escape, ×,
  // the backdrop and submit alike — and a cancel is simply the value
  // staying null.
  let importAnswer = null;
  let settleImport = null;

  // Ask for the imported microgrid's name and id. Resolves to
  // `{ name, mid }` — `mid` null for "let the server allocate" — or
  // null if the user backed out.
  //
  // `seed` re-opens the dialog after a refused import: the previous
  // answer back in the fields, the server's wording in the error
  // paragraph. Without one the dialog starts fresh.
  function askImportDetails(seed = null) {
    const dlg = document.getElementById("import-mg-dialog");
    if (!dlg) return Promise.resolve(null);
    const err = document.getElementById("import-mg-error");
    err.textContent = seed?.error || "";
    err.hidden = !seed?.error;
    document.getElementById("import-mg-name").value = seed?.name || "";
    // Pre-filled the way the create dialog pre-fills its id — the
    // lowest free one — and just as editable: the server re-checks
    // under the create lock, and a taken one comes back as a 409.
    document.getElementById("import-mg-id").value = seed
      ? seed.mid == null
        ? ""
        : String(seed.mid)
      : String(nextFreeId());
    return new Promise((resolve) => {
      importAnswer = null;
      settleImport = resolve;
      dlg.showModal();
    });
  }

  function setupImportMgDialog() {
    const dlg = document.getElementById("import-mg-dialog");
    if (!dlg) return;
    document
      .getElementById("import-mg-close")
      .addEventListener("click", () => dlg.close());
    dlg.addEventListener("click", (e) => {
      if (e.target === dlg) dlg.close();
    });
    dlg.addEventListener("close", () => {
      const settle = settleImport;
      settleImport = null;
      if (settle) settle(importAnswer);
    });
    document.getElementById("import-mg-form").addEventListener("submit", (e) => {
      e.preventDefault();
      const err = document.getElementById("import-mg-error");
      const name = document.getElementById("import-mg-name").value.trim();
      if (!name) return;
      // A blank id field means "let the server allocate" — the same
      // bargain the create dialog offers, and the only way to reach
      // the import endpoint's auto-assign path from the UI. A filled
      // one has to be a positive whole number; whether it is FREE is
      // the server's half of the answer, and comes back as the
      // dialog's inline error on the next pass.
      const raw = document.getElementById("import-mg-id").value.trim();
      let mid = null;
      if (raw !== "") {
        mid = Number(raw);
        if (!/^\d+$/.test(raw) || !Number.isSafeInteger(mid) || mid < 1) {
          err.textContent = "Microgrid id must be a positive whole number.";
          err.hidden = false;
          return;
        }
      }
      importAnswer = { name, mid };
      dlg.close();
    });
  }

  // One import flow at a time. The dialog's resolver is module-scoped
  // and an import spans two awaits it does not own the screen for —
  // the POST, and any retry after a refusal — so a second file pick
  // landing in between would clobber the first flow's resolver and
  // then call showModal() on an already-open dialog: an
  // InvalidStateError thrown after the damage, leaving the second
  // dialog open and settling nothing while the first flow dies as an
  // unhandled rejection. Guarding the whole flow makes the resolver
  // single-owner by construction, so nothing below has to juggle it.
  let importInFlight = false;

  async function importSiteFiles(files) {
    if (importInFlight) {
      notify("An import is already in progress.");
      return;
    }
    importInFlight = true;
    try {
      await runImport(files);
    } finally {
      importInFlight = false;
    }
  }

  // The site-export files are identified by content — the object key
  // names — not by their file names, so renamed downloads still
  // import. A file that parses but matches neither key is an error;
  // a file that doesn't parse at all is too.
  async function runImport(files) {
    let components = null;
    let connections = null;
    for (const file of files) {
      let parsed;
      try {
        parsed = JSON.parse(await file.text());
      } catch (e) {
        notify(`${file.name}: not valid JSON (${e.message})`);
        return;
      }
      // JSON.parse also accepts null / numbers / strings — anything
      // that isn't an object with one of the two keys is an error,
      // not a crash (`null.electricalComponents` would throw into an
      // unhandled rejection: no toast, import silently dead).
      if (parsed?.electricalComponents) {
        if (components) {
          notify(`${file.name}: second components export — pick only one`);
          return;
        }
        components = parsed;
      } else if (parsed?.electricalComponentConnections) {
        if (connections) {
          notify(`${file.name}: second connections export — pick only one`);
          return;
        }
        connections = parsed;
      } else {
        notify(`${file.name}: neither components nor connections export`);
        return;
      }
    }
    if (!components) {
      notify("Pick the components.json file (connections.json is optional).");
      return;
    }
    // The parsed exports stay in scope across retries: a refused id
    // reopens the dialog rather than ending the import, so correcting
    // it costs one edit instead of another trip through the file
    // picker (the input was cleared the moment it fired). Cancelling
    // from the reopened dialog still abandons the whole thing.
    let seed = null;
    for (;;) {
      const chosen = await askImportDetails(seed);
      if (!chosen) return;
      const body = { name: chosen.name, components, connections };
      // Omitted entirely when blank, so the server allocates.
      if (chosen.mid !== null) body.mid = chosen.mid;
      try {
        const res = await mutate("POST", "/api/microgrids/import", body);
        const m = await res.json();
        notify(
          `Imported ${m.components} components, ${m.connections} connections.`,
          "success",
        );
        warnFailedRuntime(m.id, m.runtime);
        selectMicrogrid(m.id);
        return;
      } catch (e) {
        seed = { ...chosen, error: `Import failed: ${e.message}` };
      }
    }
  }

  function renderBreadcrumb() {
    const id = readSelectedMg();
    if (id == null) return;
    const entry = cached.find((m) => m.id === id);
    if (breadcrumbNameEl()) {
      breadcrumbNameEl().textContent = entry
        ? `#${entry.id} ${entry.name || "(unnamed)"}`
        : `#${id} (unknown)`;
    }
    if (breadcrumbTsoEl()) {
      breadcrumbTsoEl().textContent = entry?.tso ? `· ${entry.tso}` : "";
    }
    renderHeaderState(entry);
  }

  // The selected microgrid's file state in its header: the same two
  // chips the cards carry, plus the Adopt button — the way out of
  // read-only, so it only shows while there is something to adopt.
  function renderHeaderState(entry) {
    const chips = document.getElementById("mg-file-chips");
    const adopt = document.getElementById("mg-adopt-btn");
    if (!chips || !adopt) return;
    const unmanaged = Boolean(entry) && !entry.managed;
    chips.innerHTML = [
      unmanaged
        ? `<span class="mg-chip unmanaged" title="${escapeHtml(UNMANAGED_HINT)}">unmanaged</span>`
        : "",
      entry?.unsaved
        ? `<span class="mg-chip unsaved" title="live edits this file could not record">unsaved</span>`
        : "",
    ].join("");
    adopt.hidden = !unmanaged;
  }

  // Take a hand-written file over: macrocosim writes the live
  // structure into it as a generated block and may rewrite it from
  // then on. Anything the block can't carry comes back as warnings,
  // which are the whole point of the round trip — surface every one.
  async function adoptCurrent() {
    const id = readSelectedMg();
    if (id == null) return;
    const btn = document.getElementById("mg-adopt-btn");
    btn.disabled = true;
    try {
      const res = await mutate("POST", `/api/mg/${id}/adopt`);
      const { warnings } = await res.json();
      notify(`Adopted microgrid #${id}.`, "success");
      for (const w of warnings || []) notify(`Adopt warning: ${w}`);
    } catch (e) {
      notify(`Adopt failed: ${e.message}`);
    } finally {
      btn.disabled = false;
    }
    await refresh();
  }

  // Shared by refresh() and the 5 s poll. A non-ok response keeps
  // the previous list. A fetch that fails outright (network-level)
  // blanks it for refresh() and keeps it for the poll
  // (`blankOnFailure`). Two fetches can be in flight at once (the
  // poll's and the one a selection write triggers), so outcomes land
  // in issue order or not at all: a list older than the one on screen
  // is dropped, and a failure blanks only while nothing newer has
  // answered at all (a newer non-ok answer is still an answer). A
  // list fresh from the server is the one place the selection can be
  // checked against what the server has — unless the selection moved
  // while the response was in flight: the user can pick a microgrid
  // the list was fetched too early to know (create, import, a card
  // click), so such a list is kept but not made a judge. The fetch
  // the selection write itself triggers, or the next poll, judges it
  // instead.
  let listIssued = 0;
  let listApplied = 0; // the request whose list is on screen
  let listAnswered = 0; // the newest request the server answered
  async function fetchList(blankOnFailure) {
    const seq = ++listIssued;
    const mg = readSelectedMg();
    let rows = null; // null: the fetch failed outright
    try {
      const res = await fetch("/api/microgrids");
      listAnswered = Math.max(listAnswered, seq);
      if (!res.ok) return;
      rows = await res.json();
    } catch (_) {
      if (!blankOnFailure || seq < listAnswered) return;
    }
    if (seq < listApplied) return;
    // A blank holds no sequence slot: an older body still in flight
    // is a real list, and repaints over it.
    if (rows) listApplied = seq;
    cached = rows ?? [];
    // Only a list fresh from the server may drop the selection.
    if (rows && readSelectedMg() === mg) reconcileSelection(rows);
  }
  function renderAll() {
    window.__mgPanelCache = cached;
    // Publish before rendering: the palette lock and the inspector's
    // read-only gate both read the flags this call installs.
    publishMgFlags(cached);
    renderList();
    renderBreadcrumb();
    renderReplMgChip();
    refreshPaletteLock();
  }

  async function refresh() {
    await fetchList(true);
    renderAll();
    schedulePoll();
  }

  function schedulePoll() {
    if (pollTimer) clearInterval(pollTimer);
    if (document.body.dataset.mode !== "microgrids") return;
    pollTimer = setInterval(async () => {
      if (document.body.dataset.mode !== "microgrids") {
        clearInterval(pollTimer);
        pollTimer = null;
        return;
      }
      // Unlike refresh(), a transient poll failure keeps the old
      // list — blanking the cards every 5 s while the server
      // restarts would just flicker.
      await fetchList(false);
      renderAll();
    }, 5000);
  }

  // Module scripts run after the DOM is parsed, so the hidden file
  // input exists by now. Wired once here — the card in renderList is
  // re-created per render and only clicks the input.
  document.getElementById("import-files").addEventListener("change", (ev) => {
    const files = [...ev.target.files];
    ev.target.value = ""; // same files pickable again
    if (files.length) importSiteFiles(files);
  });

  setupLoadScriptDialog();
  setupCreateMgDialog();
  setupImportMgDialog();
  // The header button is static markup, so one listener outlives
  // every renderBreadcrumb (which only flips its hidden flag).
  document.getElementById("mg-adopt-btn")?.addEventListener("click", adoptCurrent);
  return { refresh };
})();

// ─── Scenarios mode ─────────────────────────────────────────────────────────
//
// Driven by /api/scenarios (snapshot) + the POST endpoints for
// start / stop / next / prev / jump. Renders a 24-h horizontal
// timeline strip with one block per stage, a "now" marker pinned
// to the current local-hour, a stage-row list below, and Start /
// Prev / Next / Stop controls in the header. Pollers refresh the
// snapshot every 5 s while the mode is active — auto-advance
// transitions and journal events otherwise wouldn't update the
// timeline since they happen server-side without a WS push.
// Driven by the unified registry: /api/scenarios (the registered
// scenarios + their cue/check timeline) plus the journal readers
// /api/scenario (lifecycle), /api/scenario/report (live metrics + the
// scenario-expect ledger), and /api/scenario/events (activity feed).
// The journal tracks one scenario at a time; Run starts a scenario on
// the wall clock, Stop ends it. Headless/deterministic runs are a
// `macroctl scenario run --stepped` / CI concern, not a UI action.
export const scenariosPanel = (() => {
  let scenarios = []; // /api/scenarios snapshot
  let summary = null; // /api/scenario (running/last journal)
  let report = null; // /api/scenario/report
  let events = []; // /api/scenario/events
  let csv = { dir: null, files: [] }; // /api/scenario/csv
  let pollTimer = null;

  function listEl() { return document.getElementById("scenarios-list"); }

  // The journal holds one scenario; it's running when a name is set and
  // it hasn't ended. `journalName` is the name it currently reflects
  // (running or just-stopped) — drives the last-run badge + run view.
  function runningName() {
    return summary?.name && !summary.ended_at ? summary.name : null;
  }
  function journalName() { return summary?.name || null; }
  function scenarioByName(n) { return scenarios.find((s) => s.name === n) || null; }

  function fmtSecs(s) {
    if (s == null) return "open";
    return s < 90 ? `${Math.round(s)}s` : `${Math.round(s / 60)}min`;
  }
  function mkBtn(label, onClick) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "hdr-btn";
    b.textContent = label;
    b.addEventListener("click", onClick);
    return b;
  }

  // ── registered-scenario list ────────────────────────────────────────
  function renderList() {
    const el = listEl();
    if (!el) return;
    const countEl = document.getElementById("sc-count");
    if (countEl) countEl.textContent = scenarios.length ? `${scenarios.length} registered` : "";
    if (scenarios.length === 0) {
      el.innerHTML =
        `<p class="muted">No scenarios registered. Load a config that calls <code>define-scenario</code>.</p>`;
      return;
    }
    const running = runningName();
    el.innerHTML = "";
    for (const s of scenarios) {
      const isRunning = s.name === running;
      const sections = [
        s.has_setup ? "setup" : null,
        s.n_drive ? `drive×${s.n_drive}` : null,
        s.n_agents ? `agents×${s.n_agents}` : null,
        s.n_cues ? `cues×${s.n_cues}` : null,
        s.n_expect ? `checks×${s.n_expect}` : null,
        s.records ? "rec" : null,
      ].filter(Boolean).join(" · ");
      const row = document.createElement("div");
      row.className = "sc-row";
      if (s.name === journalName()) row.classList.add("selected");
      row.innerHTML = `
        <div class="sc-row-main">
          <span class="sc-row-name">${escapeHtml(s.name)}</span>
          <span class="sc-row-meta">${s.schedule}/${s.clock} · ${fmtSecs(s.length_s)}${
            s.seed != null ? ` · seed ${s.seed}` : ""
          }</span>
          <span class="sc-row-desc muted">${escapeHtml(s.description || "")}</span>
        </div>
        <div class="sc-row-sections muted">${sections}</div>
        <div class="sc-row-badge">${badgeHtml(s.name, isRunning)}</div>
        <div class="sc-row-actions"></div>
      `;
      const actions = row.querySelector(".sc-row-actions");
      if (isRunning) {
        actions.appendChild(mkBtn("Stop", stopRun));
      } else {
        const run = mkBtn("Run", () => startRun(s.name));
        run.disabled = !!running; // one journal at a time
        run.title = running ? `${running} is running — stop it first` : "Run live";
        actions.appendChild(run);
      }
      el.appendChild(row);
    }
  }

  function badgeHtml(name, isRunning) {
    if (isRunning) return `<span class="sc-badge running">running</span>`;
    // Last-run result lives on the single journal, so only the
    // most-recently-run scenario carries a badge.
    if (name === journalName() && summary?.ended_at && report) {
      const p = report.checks_passed || 0;
      const f = report.checks_failed || 0;
      if (p + f === 0) return `<span class="sc-badge">ran</span>`;
      return `<span class="sc-badge ${f === 0 ? "pass" : "fail"}">✓${p} ✗${f}</span>`;
    }
    return "";
  }

  // ── run view ────────────────────────────────────────────────────────
  function renderRunView() {
    const view = document.getElementById("sc-run-view");
    if (!view) return;
    const name = journalName();
    const sc = name && scenarioByName(name);
    if (!name || !sc) { view.hidden = true; return; }
    view.hidden = false;

    const running = runningName() === name;
    const elapsed = report ? report.scenario_elapsed_s : summary ? summary.elapsed_s : 0;
    document.getElementById("sc-run-name").textContent = name;
    const status = document.getElementById("sc-run-status");
    status.textContent = running ? "running" : "stopped";
    status.className = `sc-badge ${running ? "running" : ""}`;
    document.getElementById("sc-run-elapsed").textContent =
      `${Math.round(elapsed || 0)}s${sc.length_s ? ` / ${fmtSecs(sc.length_s)}` : ""}`;
    const stopBtn = document.getElementById("sc-run-stop");
    if (stopBtn) stopBtn.disabled = !running;

    renderRunTimeline(sc, elapsed || 0);
    renderMetrics();
    renderChecks();
    renderEvents();
    renderCsv();
  }

  // Cues + checks positioned over the run length; fired once elapsed
  // passes their time, and checks coloured by their recorded result.
  function renderRunTimeline(sc, elapsed) {
    const track = document.getElementById("sc-run-timeline");
    if (!track) return;
    track.innerHTML = "";
    const tl = sc.timeline || [];
    if (tl.length === 0) {
      track.innerHTML = `<span class="muted">no cues or checks</span>`;
      return;
    }
    const span = sc.length_s || Math.max(...tl.map((t) => t.at_s), 1);
    // Recorded checks arrive oldest-first; correlate to timeline checks
    // (also oldest-first) by position.
    const reportChecks = report?.checks || [];
    let checkIdx = 0;
    for (const t of tl) {
      const dot = document.createElement("div");
      dot.className = `sc-tl-mark sc-tl-${t.kind}`;
      dot.style.left = `${Math.min(100, (t.at_s / span) * 100).toFixed(1)}%`;
      let state = elapsed >= t.at_s ? "fired" : "pending";
      let detail = "";
      if (t.kind === "check") {
        const rc = reportChecks[checkIdx++];
        if (rc) {
          state = rc.passed ? "pass" : "fail";
          detail = ` — ${rc.expectation}${rc.actual != null ? ` (got ${rc.actual})` : ""}`;
        }
      }
      dot.classList.add(`sc-tl-${state}`);
      dot.title = `${t.label} @${t.at_s}s${detail}`;
      track.appendChild(dot);
    }
  }

  function renderMetrics() {
    const el = document.getElementById("sc-run-metrics");
    if (!el) return;
    if (!report) { el.innerHTML = ""; return; }
    const rows = [
      ["peak import", `${(report.peak_grid_w / 1000).toFixed(1)} kW`],
      ["battery charged", `${report.total_battery_charged_wh.toFixed(0)} Wh`],
      ["battery discharged", `${report.total_battery_discharged_wh.toFixed(0)} Wh`],
      ["PV produced", `${report.total_pv_produced_wh.toFixed(0)} Wh`],
    ];
    if (report.soc_stats) rows.push(["SoC mean", `${report.soc_stats.mean_pct.toFixed(0)}%`]);
    el.innerHTML =
      `<h3>metrics</h3>` +
      rows.map(([k, v]) => `<div class="sc-metric"><span>${k}</span><b>${v}</b></div>`).join("");
  }

  function renderChecks() {
    const el = document.getElementById("sc-run-checks");
    if (!el) return;
    const checks = report?.checks || [];
    const head = `<h3>checks ${report ? `(${report.checks_passed}✓ ${report.checks_failed}✗)` : ""}</h3>`;
    if (checks.length === 0) { el.innerHTML = `${head}<p class="muted">none yet</p>`; return; }
    const rows = checks.map((c) =>
      `<div class="sc-check ${c.passed ? "pass" : "fail"}">
        <span>${c.passed ? "✓" : "✗"}</span>
        <span>#${c.component_id} ${escapeHtml(c.metric)}</span>
        <span class="muted">${escapeHtml(c.expectation)}${c.actual != null ? ` · got ${c.actual}` : ""}</span>
      </div>`).join("");
    el.innerHTML = `${head}${rows}`;
  }

  function renderEvents() {
    const el = document.getElementById("sc-run-events");
    if (!el) return;
    if (events.length === 0) { el.innerHTML = `<h3>events</h3><p class="muted">none</p>`; return; }
    const recent = events.slice(-12).reverse();
    el.innerHTML =
      `<h3>events</h3>` +
      recent.map((e) =>
        `<div class="sc-event"><code>${escapeHtml(e.kind)}</code> ${escapeHtml(String(e.payload))}</div>`,
      ).join("");
  }

  function renderCsv() {
    const el = document.getElementById("sc-run-csv");
    if (!el) return;
    if (!csv.files || csv.files.length === 0) { el.innerHTML = ""; return; }
    const links = csv.files.map((f) =>
      `<a class="sc-csv-link" href="/api/scenario/csv/${encodeURIComponent(f)}" download>${escapeHtml(f)}</a>`,
    ).join("");
    el.innerHTML = `<h3>recorded csv${csv.dir ? ` <span class="muted">${escapeHtml(csv.dir)}</span>` : ""}</h3>${links}`;
  }

  function updateActiveChip() {
    const chip = document.getElementById("active-scenarios");
    if (!chip) return;
    const name = runningName();
    if (!name) { chip.hidden = true; chip.textContent = ""; return; }
    chip.hidden = false;
    chip.textContent = `running: ${name}`;
    chip.title = "Click to view it in Scenarios mode";
  }

  // ── actions ─────────────────────────────────────────────────────────
  async function startRun(name) {
    try {
      await mutate("POST", `/api/scenarios/${encodeURIComponent(name)}/start`);
    } catch (err) {
      notify(`run failed: ${err.message}`);
    }
    await refresh();
  }
  async function stopRun() {
    try {
      await mutate("POST", "/api/scenarios/stop");
    } catch (err) {
      notify(`stop failed: ${err.message}`);
    }
    await refresh();
  }

  function render() {
    renderList();
    renderRunView();
    updateActiveChip();
  }

  const getJson = (path) => fetch(path).then((r) => r.json());
  const settled = (r, fallback) => (r.status === "fulfilled" ? r.value : fallback);

  async function refresh() {
    // Two stages of concurrent fetches: the journal reads depend on
    // the summary's name, nothing else depends on anything — so
    // latency is two round-trips, not five.
    const [sc, sum] = await Promise.allSettled([getJson("/api/scenarios"), getJson("/api/scenario")]);
    scenarios = settled(sc, []);
    summary = settled(sum, null);
    if (journalName()) {
      const [rep, ev, cs] = await Promise.allSettled([
        getJson("/api/scenario/report"),
        getJson("/api/scenario/events?limit=50"),
        getJson("/api/scenario/csv"),
      ]);
      report = settled(rep, null);
      events = settled(ev, {}).events || [];
      csv = settled(cs, { dir: null, files: [] });
    } else {
      report = null;
      events = [];
      csv = { dir: null, files: [] };
    }
    render();
    schedulePoll();
  }

  // Keep the header chip live from the one summary fetch when the
  // Scenarios mode is hidden — the full 5-fetch refresh only runs
  // while its panels are actually showing.
  async function refreshChip() {
    try {
      summary = await getJson("/api/scenario");
    } catch (_) {
      summary = null;
    }
    updateActiveChip();
  }

  function schedulePoll() {
    if (pollTimer) return;
    // Server-side scenario time + checks have no WS push, so the run
    // view stays live on a 3 s poll while the mode shows. Hidden, only
    // the header chip needs a signal — one summary fetch, and at a
    // 15 s cadence: a scenario starting elsewhere is a coarse event,
    // not something worth polling at run-view rates forever.
    let tick = 0;
    pollTimer = setInterval(() => {
      if (document.body.dataset.mode === "scenarios") refresh();
      else if (++tick % 5 === 0) refreshChip();
    }, 3000);
  }

  function setup() {
    document.getElementById("sc-run-stop")?.addEventListener("click", stopRun);
    document.getElementById("active-scenarios")?.addEventListener("click", () => {
      document.querySelector("#mode-toggle .mode-btn[data-mode='scenarios']")?.click();
    });
    refresh();
  }
  return { setup, refresh };
})();
