// Reading failed API responses. Every route fails with JSON
// `{"error": "..."}`; anything else (a proxy page, an empty body)
// falls back to its text or the status line.

// The message of a failed response.
export async function errorText(res) {
  const text = await res.text().catch(() => "");
  try {
    const body = JSON.parse(text);
    if (body && typeof body.error === "string") return body.error;
  } catch (_e) {
    // Not JSON: fall through to the raw text.
  }
  return text.trim() || `HTTP ${res.status}`;
}

// The JSON body of a GET of `path`; a failed response throws its
// message.
export async function getJson(path) {
  const res = await fetch(path);
  if (!res.ok) throw new Error(await errorText(res));
  return res.json();
}
