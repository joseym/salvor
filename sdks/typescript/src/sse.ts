/**
 * A browser-safe server-sent-events parser over a fetch body stream.
 *
 * Both the event tail (`GET .../events`, a GET that returns SSE) and the
 * streaming model step (`POST .../model-step`, a POST that returns SSE) speak
 * the same line protocol, so one parser serves both. It reads the fetch
 * `Response.body` stream directly with `TextDecoder`, using no Node-only API,
 * so it runs unchanged in a browser. A browser `EventSource` cannot be used for
 * the model step because that is a POST, which is why the SDK parses the fetch
 * body itself here.
 */

/** One parsed frame: its `event:` name (absent means the default `message`)
 * and its assembled `data:` payload. */
export interface SseFrame {
  event?: string;
  data: string;
}

/**
 * Read a fetch body stream, yielding one {@link SseFrame} per server-sent
 * frame. Frames are separated by a blank line; `event:` and `data:` fields
 * accumulate until then. A frame carrying no `data:` line is skipped.
 */
export async function* readSseFrames(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<SseFrame> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let sep: number;
    while ((sep = buffer.indexOf("\n\n")) !== -1) {
      const raw = buffer.slice(0, sep);
      buffer = buffer.slice(sep + 2);
      const frame = parseSseBlock(raw);
      if (frame) yield frame;
    }
  }
}

/** Parse one SSE frame's text block into a {@link SseFrame}, or null to skip
 * a block with no `data:` line (a lone comment or keep-alive). */
export function parseSseBlock(raw: string): SseFrame | null {
  let event: string | undefined;
  const dataLines: string[] = [];
  for (const line of raw.split("\n")) {
    if (line === "" || line.startsWith(":")) continue; // blank or comment
    const idx = line.indexOf(":");
    const field = idx === -1 ? line : line.slice(0, idx);
    let value = idx === -1 ? "" : line.slice(idx + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") event = value;
    else if (field === "data") dataLines.push(value);
    // `id:` carries the seq; the seq is also inside the data envelope.
  }
  if (dataLines.length === 0) return null;
  return { event, data: dataLines.join("\n") };
}
