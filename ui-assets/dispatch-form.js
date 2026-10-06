// New-dispatch dialog: a modal form over POST /api/mg/{id}/dispatches.
//
// Picks over typing wherever the data is known: target categories /
// components come from the microgrid's own topology, start time is a
// native datetime picker, duration is value + unit, recurrence is
// frequency + every-N, and the payload is key → value rows instead of
// raw JSON. The only free-text field is the dispatch type, which the
// API defines as consumer-owned vocabulary.
//
// `setup({ onCreated })` wires the static listeners once;
// `open(mgId)` resets the form, loads the microgrid's components,
// and shows the dialog.

import { escapeHtml, mutate, notify } from "./app.js";
import { ACCEPTS_SETPOINTS } from "./inspect.js";
import { mgJson, readSelectedMg } from "./routing.js";

// The server's duration_s / recurrence interval are u32 — validate
// here so the user gets a readable message instead of the server's
// JSON deserialization error.
const U32_MAX = 4294967295;

export const dispatchForm = (() => {
  let currentMg = null;
  let onCreated = () => {};
  // Bumped on every open(); a submit captures it so a slow POST's
  // continuation can tell whether the user dismissed the dialog and
  // started a fresh session while the request was in flight.
  let session = 0;
  const $ = (id) => document.getElementById(id);
  const dlg = () => $("dispatch-dialog");

  // ── target ────────────────────────────────────────────────────

  function targetMode() {
    return dlg().querySelector(".dd-seg-btn.active")?.dataset.mode || "categories";
  }

  function setTargetMode(mode) {
    for (const b of dlg().querySelectorAll(".dd-seg-btn")) {
      b.classList.toggle("active", b.dataset.mode === mode);
    }
    $("dd-target-categories").hidden = mode !== "categories";
    $("dd-target-components").hidden = mode !== "components";
    $("dd-target-hint").textContent =
      mode === "categories"
        ? "Controllable categories present in this microgrid."
        : "Controllable components in this microgrid.";
  }

  // Build the chip row + component list from the microgrid's
  // topology. Hidden components are synthetic loads — not
  // addressable, so they don't appear in either mode.
  function populateTargets(components) {
    const visible = components.filter(
      (c) => !c.hidden && ACCEPTS_SETPOINTS.has(c.category),
    );
    const byCat = new Map();
    for (const c of visible) {
      if (!byCat.has(c.category)) byCat.set(c.category, []);
      byCat.get(c.category).push(c);
    }

    const chips = $("dd-target-categories");
    chips.innerHTML = "";
    for (const [cat, list] of byCat) {
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = "dd-chip";
      // parse_target accepts the topology's category names verbatim
      // (including "ev-charger"), so no alias mapping is needed.
      chip.dataset.cat = cat;
      chip.innerHTML = `${escapeHtml(cat)} <span class="dd-chip-n">×${list.length}</span>`;
      chip.addEventListener("click", () => chip.classList.toggle("active"));
      chips.appendChild(chip);
    }

    const comps = $("dd-target-components");
    comps.innerHTML = "";
    if (byCat.size === 0) {
      const empty = '<p class="dd-hint">No controllable components in this microgrid.</p>';
      chips.innerHTML = empty;
      comps.innerHTML = empty;
    }
    for (const [cat, list] of byCat) {
      const head = document.createElement("div");
      head.className = "dd-comp-group";
      head.textContent = cat;
      comps.appendChild(head);
      for (const c of list) {
        const row = document.createElement("label");
        row.className = "dd-comp";
        row.innerHTML = `
          <input type="checkbox" value="${c.id}" />
          <span class="dd-comp-id">#${c.id}</span>
          <span>${escapeHtml(c.name || "")}</span>`;
        comps.appendChild(row);
      }
    }
  }

  function targetSpec() {
    if (targetMode() === "categories") {
      const picked = [...$("dd-target-categories").querySelectorAll(".dd-chip.active")];
      return picked.map((c) => c.dataset.cat).join(",");
    }
    const picked = [...$("dd-target-components").querySelectorAll("input:checked")];
    return picked.map((i) => i.value).join(",");
  }

  // ── payload rows ──────────────────────────────────────────────

  function addPayloadRow(key = "", value = "") {
    const row = document.createElement("div");
    row.className = "dd-kv-row";
    row.innerHTML = `
      <input class="dd-input dd-input-inline dd-kv-key" placeholder="key" />
      <span class="dd-kv-arrow">→</span>
      <input class="dd-input dd-input-inline dd-kv-value" placeholder="value" />
      <button type="button" class="dd-kv-del" title="Remove field">×</button>`;
    row.querySelector(".dd-kv-key").value = key;
    row.querySelector(".dd-kv-value").value = value;
    row.querySelector(".dd-kv-del").addEventListener("click", () => row.remove());
    $("dd-payload-rows").appendChild(row);
  }

  // Collect the key → value rows into a payload object. Values that
  // parse as JSON keep their type (5 → number, true → bool,
  // [1,2] → array); anything else rides as a string. Returns
  // undefined when every row is blank.
  function payloadObject() {
    // Null prototype + hasOwn: a plain `{}` with `key in out` would
    // report keys like "toString" as duplicates of the inherited
    // Object.prototype members, and `out["__proto__"] = v` would
    // silently set the prototype instead of an own property.
    const out = Object.create(null);
    for (const row of $("dd-payload-rows").querySelectorAll(".dd-kv-row")) {
      const key = row.querySelector(".dd-kv-key").value.trim();
      const raw = row.querySelector(".dd-kv-value").value.trim();
      if (!key && !raw) continue; // blank row — ignore
      if (!key) throw new Error("payload: a value is missing its key");
      if (Object.hasOwn(out, key)) throw new Error(`payload: duplicate key "${key}"`);
      let value = raw;
      try {
        value = JSON.parse(raw);
      } catch (_) {
        /* not JSON — keep the raw string */
      }
      out[key] = value;
    }
    return Object.keys(out).length ? { ...out } : undefined;
  }

  // ── form state ────────────────────────────────────────────────

  function resetForm() {
    $("dispatch-form").reset();
    // The submit button is shared across sessions; a previous
    // session's still-in-flight submit left it disabled. form.reset()
    // doesn't touch `disabled`, so a fresh session must.
    dlg().querySelector('button[type="submit"]').disabled = false;
    setTargetMode("categories");
    $("dd-payload-rows").innerHTML = "";
    addPayloadRow();
    $("dd-recur-every").hidden = true;
    $("dd-start-at").disabled = true;
    $("dd-duration-value").disabled = true;
    $("dd-duration-unit").disabled = true;
    showError("");
  }

  function showError(message) {
    const el = $("dd-error");
    el.textContent = message;
    el.hidden = !message;
  }

  function buildBody() {
    const type = $("dd-type").value.trim();
    if (!type) throw new Error("type is required");
    const target = targetSpec();
    if (!target) {
      throw new Error(
        targetMode() === "categories"
          ? "pick at least one target category"
          : "pick at least one target component",
      );
    }
    const body = {
      type,
      target,
      active: $("dd-active").checked,
      dry_run: $("dd-dry-run").checked,
    };

    if (dlg().querySelector('input[name="dd-start-mode"]:checked').value === "at") {
      const ms = new Date($("dd-start-at").value).getTime();
      if (!Number.isFinite(ms)) {
        throw new Error("pick a start time, or choose Immediately");
      }
      body.start_ms = ms;
    }

    if (dlg().querySelector('input[name="dd-duration-mode"]:checked').value === "for") {
      const value = Number($("dd-duration-value").value);
      // 0 is a valid "instant" dispatch (the list renders it as
      // such); negatives and blanks are not.
      if (!Number.isFinite(value) || value < 0 || $("dd-duration-value").value === "") {
        throw new Error("duration must be a non-negative number");
      }
      const seconds = Math.floor(value * Number($("dd-duration-unit").value));
      if (seconds > U32_MAX) throw new Error("duration is too large");
      body.duration_s = seconds;
    }

    const freq = $("dd-recur-freq").value;
    if (freq !== "once") {
      const interval = Math.floor(Number($("dd-recur-interval").value));
      if (!Number.isFinite(interval) || interval < 1) {
        throw new Error("repeat interval must be at least 1");
      }
      if (interval > U32_MAX) throw new Error("repeat interval is too large");
      body.recurrence = { freq, interval };
    }

    const payload = payloadObject();
    if (payload !== undefined) body.payload = payload;
    return body;
  }

  async function submit() {
    let body;
    try {
      body = buildBody();
    } catch (err) {
      showError(err.message);
      return;
    }
    const mg = currentMg;
    const mySession = session;
    // Disable the submit button for the duration of the POST so a
    // double-click / double-Enter can't create the dispatch twice.
    const submitBtn = dlg().querySelector('button[type="submit"]');
    submitBtn.disabled = true;
    try {
      await mutate("POST", `/api/mg/${mg}/dispatches`, body);
    } catch (err) {
      // If the user dismissed the dialog (Esc / Cancel stay live
      // during the POST) — or reopened it for a fresh dispatch —
      // dd-error belongs to the new session; toast instead.
      if (session === mySession && dlg().open) {
        showError(err.message);
      } else {
        notify(`create failed: ${err.message}`);
      }
      return;
    } finally {
      // Only re-enable for our own session — a fresh session may
      // have its own submit in flight on this same button by now.
      if (session === mySession) submitBtn.disabled = false;
    }
    // The dispatch exists regardless — always toast and refresh the
    // list; only close the dialog if it's still this submission's
    // session (not a fresh form the user has started filling in).
    if (session === mySession) dlg().close();
    notify("dispatch created", "success");
    onCreated(mg);
  }

  // ── wiring ────────────────────────────────────────────────────

  function setup(opts = {}) {
    onCreated = opts.onCreated || (() => {});
    const d = dlg();
    if (!d) return;

    for (const b of d.querySelectorAll(".dd-seg-btn")) {
      b.addEventListener("click", () => setTargetMode(b.dataset.mode));
    }

    // Radio modes enable/disable their dependent inputs; picking a
    // dependent input's mode happens implicitly via the labels.
    // change only fires on the radio that just became checked, so
    // r.value alone identifies the active mode.
    for (const r of d.querySelectorAll('input[name="dd-start-mode"]')) {
      r.addEventListener("change", () => {
        const on = r.value === "at";
        $("dd-start-at").disabled = !on;
        if (on) $("dd-start-at").focus();
      });
    }
    for (const r of d.querySelectorAll('input[name="dd-duration-mode"]')) {
      r.addEventListener("change", () => {
        const on = r.value === "for";
        $("dd-duration-value").disabled = !on;
        $("dd-duration-unit").disabled = !on;
        if (on) $("dd-duration-value").focus();
      });
    }

    // "every 1 days" reads badly — singularise the unit on 1. The
    // unit names live on the <option data-unit> attributes so the
    // frequency vocabulary stays in one place (the markup).
    const updateRecurUnit = () => {
      const unit = $("dd-recur-freq").selectedOptions[0]?.dataset.unit || "";
      const n = Number($("dd-recur-interval").value);
      $("dd-recur-unit").textContent = n === 1 ? unit.replace(/s$/, "") : unit;
    };
    $("dd-recur-freq").addEventListener("change", () => {
      $("dd-recur-every").hidden = $("dd-recur-freq").value === "once";
      updateRecurUnit();
    });
    $("dd-recur-interval").addEventListener("input", updateRecurUnit);

    $("dd-payload-add").addEventListener("click", () => addPayloadRow());

    $("dispatch-form").addEventListener("submit", (e) => {
      e.preventDefault();
      submit();
    });
    $("dd-cancel").addEventListener("click", () => d.close());
    $("dispatch-dialog-close").addEventListener("click", () => d.close());
    // Click-outside-to-dismiss, mirroring the other dialogs.
    d.addEventListener("click", (e) => {
      if (e.target === d) d.close();
    });
  }

  let opening = false;

  async function open(mgId) {
    // Re-entrancy guard: a double-click on the New-dispatch button
    // would otherwise run two concurrent opens, and the second
    // showModal() on an already-open dialog throws.
    // The form posts to mgId but loads the selected microgrid's
    // topology, so the two must agree.
    if (mgId == null || mgId !== readSelectedMg() || opening || dlg().open) return;
    opening = true;
    session += 1;
    currentMg = mgId;
    resetForm();
    try {
      const topo = await mgJson("topology");
      populateTargets(topo.components || []);
    } catch (err) {
      notify(`couldn't load topology: ${err.message}`);
      return;
    } finally {
      opening = false;
    }
    dlg().showModal();
    $("dd-type").focus();
  }

  return { setup, open };
})();
