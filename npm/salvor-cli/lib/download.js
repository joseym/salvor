"use strict";

// Plain http(s) GET with redirect and HTTP(S)_PROXY support, no dependency.
// Carried over unchanged from the cargo-dist-generated installer this
// package replaces (see npm/salvor-cli/README-internal.md).

const https = require("node:https");
const http = require("node:http");

function getProxyForUrl(urlString) {
  const url = new URL(urlString);
  const isHttps = url.protocol === "https:";

  const noProxy = process.env.NO_PROXY || process.env.no_proxy || "";
  if (noProxy === "*") return null;
  if (noProxy) {
    const hostname = url.hostname.toLowerCase();
    const noProxyList = noProxy.split(",").map((s) => s.trim().toLowerCase());
    for (const entry of noProxyList) {
      if (hostname === entry || hostname.endsWith("." + entry)) {
        return null;
      }
    }
  }

  const proxyEnv = isHttps
    ? process.env.HTTPS_PROXY || process.env.https_proxy
    : process.env.HTTP_PROXY || process.env.http_proxy;

  if (!proxyEnv) return null;

  const proxyUrl = new URL(proxyEnv);

  let auth = null;
  if (proxyUrl.username || proxyUrl.password) {
    auth = `${proxyUrl.username}:${proxyUrl.password}`;
  }

  return {
    hostname: proxyUrl.hostname,
    port: proxyUrl.port || (proxyUrl.protocol === "https:" ? 443 : 80),
    auth: auth,
  };
}

function connectThroughProxy(proxy, target) {
  return new Promise((resolve, reject) => {
    const headers = {};
    if (proxy.auth) {
      headers["Proxy-Authorization"] =
        "Basic " + Buffer.from(proxy.auth).toString("base64");
    }

    const connectReq = http.request({
      hostname: proxy.hostname,
      port: proxy.port,
      method: "CONNECT",
      path: `${target.hostname}:${target.port || 443}`,
      headers,
    });
    connectReq.on("connect", (res, socket) => {
      if (res.statusCode === 200) {
        resolve(socket);
      } else {
        reject(new Error(`Proxy CONNECT failed with status ${res.statusCode}`));
      }
    });
    connectReq.on("error", reject);
    connectReq.end();
  });
}

function download(urlString, maxRedirects) {
  if (maxRedirects === undefined) maxRedirects = 5;
  return new Promise((resolve, reject) => {
    if (maxRedirects < 0) {
      return reject(new Error("Too many redirects"));
    }

    const parsed = new URL(urlString);
    const isHttps = parsed.protocol === "https:";
    const mod = isHttps ? https : http;
    const proxy = getProxyForUrl(urlString);

    const doRequest = (extraOptions) => {
      const options = Object.assign(
        {
          hostname: parsed.hostname,
          port: parsed.port || (isHttps ? 443 : 80),
          path: parsed.pathname + parsed.search,
          method: "GET",
          headers: { "User-Agent": "salvor-npm-installer" },
        },
        extraOptions || {},
      );

      if (proxy && !isHttps) {
        options.hostname = proxy.hostname;
        options.port = proxy.port;
        options.path = urlString;
        if (proxy.auth) {
          options.headers["Proxy-Authorization"] =
            "Basic " + Buffer.from(proxy.auth).toString("base64");
        }
      }

      const req = mod.request(options, (res) => {
        if (
          res.statusCode >= 300 &&
          res.statusCode < 400 &&
          res.headers.location
        ) {
          res.resume();
          const nextUrl = new URL(res.headers.location, urlString).toString();
          return download(nextUrl, maxRedirects - 1).then(resolve, reject);
        }
        if (res.statusCode < 200 || res.statusCode >= 300) {
          res.resume();
          return reject(new Error(`HTTP ${res.statusCode} from ${urlString}`));
        }
        resolve(res);
      });
      req.on("error", reject);
      req.end();
    };

    if (proxy && isHttps) {
      connectThroughProxy(proxy, parsed).then(
        (socket) => doRequest({ socket, agent: false }),
        reject,
      );
    } else {
      doRequest();
    }
  });
}

module.exports = { download };
