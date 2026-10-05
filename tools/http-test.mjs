// The error reader returns the server's `error` field, the raw text
// for a non-JSON body, and the status line for an empty one.
// Run: node tools/http-test.mjs   (exits non-zero on failure)
import assert from "node:assert/strict";
import { errorText } from "../ui-assets/http.js";

assert.equal(
  await errorText(new Response('{"error":"microgrid 9 not registered"}', { status: 404 })),
  "microgrid 9 not registered",
);
assert.equal(await errorText(new Response("<html>bad gateway</html>", { status: 502 })), "<html>bad gateway</html>");
assert.equal(await errorText(new Response("", { status: 502 })), "HTTP 502");
assert.equal(await errorText(new Response('{"other":1}', { status: 500 })), '{"other":1}');

console.log("http-test: all assertions passed");
