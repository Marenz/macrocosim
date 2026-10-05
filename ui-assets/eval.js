import { notify } from "./app.js";
import { errorText } from "./http.js";
import { mgPath } from "./routing.js";

export function jsToLispString(s) {
  return s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

// POST one Lisp expression to the selected microgrid's eval, or the
// whole-site /api/eval with none selected. `label` is the notify
// prefix on failure (defaults to the expression itself — pass
// something short like "Paste failed" when the expression would be
// unreadable in a toast). Returns { ok: true, value } on success, or
// { ok: false, error } when the evaluation failed, the server
// refused the request or the transport broke. Undo history is the
// server's: a structural eval stacks the microgrid file's previous
// generated block by itself, so there is nothing to record here.
export async function evalQuoted(expr, label = expr) {
  let res;
  try {
    res = await fetch(mgPath("eval") ?? "/api/eval", { method: "POST", body: expr });
  } catch (err) {
    notify(`${label}: ${err.message}`);
    return { ok: false, error: err.message };
  }
  if (!res.ok) {
    const error = await errorText(res);
    notify(`${label}: ${error}`);
    return { ok: false, error };
  }
  const data = await res.json();
  return { ok: true, value: data.value };
}
