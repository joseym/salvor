// A bookmarks-manager MCP server, in one file of plain ESM JavaScript.
//
// New to the Model Context Protocol? Here is the whole idea: an MCP server is
// an ordinary program that advertises a few named functions ("tools") over
// stdin/stdout. A host (here, a Salvor agent) launches this program as a child
// process, asks it what tools it has, and calls them on the model's behalf.
// Your function *is* the tool. There is no Salvor package to import, no binding
// to build, no SDK to learn beyond @modelcontextprotocol/sdk itself. That is
// the point of this example: a TypeScript or JavaScript developer extends a
// Salvor agent purely by writing one of these servers.
//
// This file is `.mjs` on purpose: it is the lowest-friction runnable form.
// Node executes it directly with no compile step and no `tsx` to download; the
// only dependencies are the MCP SDK and zod, both installed with `npm install`.
//
// `McpServer.registerTool` is the current SDK API (verified against the
// installed 1.29.0). You give it a name, a config object (description, a zod
// input schema, and side-effect `annotations`), and a callback. The zod schema
// becomes the JSON Schema the model sees; the callback's return value becomes
// the tool result.

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { appendFileSync, existsSync, readFileSync } from "node:fs";

// The store is one JSON object per line (JSONL). Its path comes from an
// environment variable so the host controls where state lands; agent.toml sets
// it. A default keeps the server runnable on its own for a quick poke.
const STORE = process.env.BOOKMARKS_FILE ?? "bookmarks.jsonl";

const server = new McpServer({ name: "bookmarks", version: "0.1.0" });

// Reads every stored bookmark. Shared by the two read tools below.
function readBookmarks() {
  if (!existsSync(STORE)) return [];
  return readFileSync(STORE, "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line));
}

server.registerTool(
  "save_bookmark",
  {
    description: "Save a bookmark. Appends it to the store and confirms.",
    inputSchema: {
      url: z.string().describe("The URL to bookmark."),
      title: z.string().describe("A short human title for the page."),
      tags: z.array(z.string()).default([]).describe("Optional topic tags."),
    },
    // idempotentHint is set true on purpose, to set up the hazard the README
    // walks through: appending a line is NOT actually idempotent (a retry saves
    // a second, duplicate bookmark), so the operator pins this tool to Write in
    // agent.toml rather than trusting the optimistic hint.
    annotations: { idempotentHint: true },
  },
  ({ url, title, tags }) => {
    appendFileSync(STORE, JSON.stringify({ url, title, tags }) + "\n");
    return { content: [{ type: "text", text: `Saved "${title}".` }] };
  },
);

server.registerTool(
  "list_bookmarks",
  {
    description: "List every saved bookmark as `title  url  [tags]` lines.",
    inputSchema: {},
    // A pure read. readOnlyHint=true lets Salvor classify it as Read on its own,
    // so agent.toml needs no override for it.
    annotations: { readOnlyHint: true },
  },
  () => {
    const marks = readBookmarks();
    if (marks.length === 0) {
      return { content: [{ type: "text", text: "No bookmarks saved yet." }] };
    }
    const text = marks
      .map((m) => `${m.title}\t${m.url}\t[${(m.tags ?? []).join(", ")}]`)
      .join("\n");
    return { content: [{ type: "text", text }] };
  },
);

server.registerTool(
  "find_bookmark",
  {
    description:
      "Find saved bookmarks whose title, url, or tags contain a query string.",
    inputSchema: {
      query: z.string().describe("Substring to match, case-insensitive."),
    },
    annotations: { readOnlyHint: true },
  },
  ({ query }) => {
    const needle = query.toLowerCase();
    const hits = readBookmarks().filter((m) =>
      `${m.title} ${m.url} ${(m.tags ?? []).join(" ")}`
        .toLowerCase()
        .includes(needle),
    );
    if (hits.length === 0) {
      return { content: [{ type: "text", text: `No match for "${query}".` }] };
    }
    const text = hits.map((m) => `${m.title}\t${m.url}`).join("\n");
    return { content: [{ type: "text", text }] };
  },
);

// Serve over stdio: read requests on stdin, write responses on stdout. This is
// the transport a Salvor agent spawns the server with.
await server.connect(new StdioServerTransport());
