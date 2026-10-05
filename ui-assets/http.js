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
