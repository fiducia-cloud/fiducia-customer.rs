// Puppeteer browser E2E: boots the real axum customer portal against the stub
// Supabase + stub fiducia-auth and drives the passwordless login journey, the
// session-cookie and CSRF boundaries, and the WS/SSE refresh streams.
import assert from "node:assert/strict";
import { test } from "node:test";
import puppeteer from "puppeteer";
import {
  chromeExecutablePath,
  CUSTOMER,
  startCustomer,
  STUB_OTP_CODE,
  unavailableReason,
} from "./customer-browser-harness.mjs";

test("puppeteer drives the customer portal login, CSRF boundary, and live streams", async (t) => {
  const unavailable = unavailableReason();
  if (unavailable) {
    t.skip(unavailable);
    return;
  }
  const server = await startCustomer();
  t.after(() => server.stop());

  const browser = await puppeteer.launch({
    executablePath: chromeExecutablePath(),
    headless: "new",
  });
  t.after(() => browser.close());

  const page = await browser.newPage();
  await page.setViewport({ height: 900, width: 1440 });
  const pageErrors = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));

  // Login page: Maud SSR renders all three first-factor forms, and the vendored
  // htmx asset is both served correctly and actually executing in the page.
  await page.goto(`${server.url}/login`, { waitUntil: "networkidle0" });
  assert.match(await pageText(page), /Sign in to Fiducia/);
  assert.match(await pageText(page), /Email a sign-in code/);
  const htmxAsset = await page.evaluate(async () => {
    const response = await fetch("/assets/htmx.min.js");
    return { ok: response.ok, contentType: response.headers.get("content-type") };
  });
  assert.equal(htmxAsset.ok, true);
  assert.match(htmxAsset.contentType ?? "", /javascript/);
  await page.waitForFunction(() => typeof window.htmx !== "undefined");

  // Progressive enhancement: submitting the email-OTP form swaps the body in
  // place via htmx. A surviving window marker proves no full navigation ran —
  // the same form would still work no-JS through its method/action fallback.
  await page.evaluate(() => {
    window.__fiduciaNoReload = true;
  });
  await page.type("#magic-email", CUSTOMER.email);
  await page.click('form:has(#magic-email) button[type="submit"]');
  await page.waitForFunction(() =>
    document.body.textContent?.includes("Check your email"),
  );
  assert.equal(
    await page.evaluate(() => window.__fiduciaNoReload === true),
    true,
    "htmx must swap the OTP page in place, not navigate",
  );

  // Redeem the stub's fixed one-time code: /login/verify finalizes against the
  // stub fiducia-auth and 303s to /app, which htmx follows and swaps in.
  await page.type("#otp-code", STUB_OTP_CODE);
  await page.click('form[action="/login/verify"] button[type="submit"]');
  await page.waitForFunction(() =>
    document.body.textContent?.includes("Fiducia Customer Portal"),
  );

  // The issued session cookie is HttpOnly + SameSite=Strict (debug build:
  // unprefixed name, non-Secure over loopback http).
  const cookies = await page.cookies(server.url);
  const session = cookies.find((cookie) => cookie.name === "fiducia_customer_session");
  assert.ok(session, "session cookie must be set after OTP login");
  assert.equal(session.httpOnly, true);
  assert.equal(session.sameSite, "Strict");

  // The ambient cookie now authenticates a full navigation to the portal.
  await page.goto(`${server.url}/app`, { waitUntil: "networkidle0" });
  assert.match(await pageText(page), /Dashboard/);
  assert.match(await pageText(page), new RegExp(CUSTOMER.email));

  // CSRF negative paths: a mutating request without a valid token is rejected
  // both pre-session (login flow nonce) and on the authenticated surface.
  const rejected = await page.evaluate(async () => {
    const post = async (path, fields) => {
      const response = await fetch(path, {
        method: "POST",
        body: new URLSearchParams(fields),
      });
      return { status: response.status, body: await response.json() };
    };
    return {
      login: await post("/login/otp", {
        csrf_token: "forged",
        method: "email",
        identifier: "dev@acme.com",
      }),
      session: await post("/app/notifications/read", {
        csrf_token: "forged",
        id: "00000000-0000-4000-8000-000000000009",
      }),
    };
  });
  assert.equal(rejected.login.status, 403);
  assert.equal(rejected.login.body.error, "customer_request_rejected");
  assert.equal(rejected.session.status, 403);
  assert.equal(rejected.session.body.error, "customer_request_rejected");

  // WebSocket /app/ws: connects under the cookie + exact-origin gate, announces
  // itself, and answers the JSON heartbeat ping with a pong.
  const ws = await page.evaluate(
    () =>
      new Promise((resolve, reject) => {
        const socket = new WebSocket(`ws://${location.host}/app/ws`);
        const messages = [];
        const timer = setTimeout(() => reject(new Error("websocket timed out")), 15000);
        socket.onmessage = (event) => {
          messages.push(JSON.parse(event.data));
          if (messages.length === 1) socket.send("ping");
          const pong = messages.find((message) => message.kind === "pong");
          if (pong) {
            clearTimeout(timer);
            socket.close();
            resolve({ first: messages[0], pong });
          }
        };
        socket.onerror = () => {
          clearTimeout(timer);
          reject(new Error("websocket error"));
        };
      }),
  );
  assert.equal(ws.first.kind, "connected");
  assert.equal(ws.first.transport, "websocket");
  assert.equal(ws.pong.kind, "pong");

  // SSE /app/events: the stream delivers its named refresh event immediately.
  const sse = await page.evaluate(
    () =>
      new Promise((resolve, reject) => {
        const source = new EventSource("/app/events");
        const timer = setTimeout(() => {
          source.close();
          reject(new Error("sse timed out"));
        }, 15000);
        source.addEventListener("fiducia-refresh", (event) => {
          clearTimeout(timer);
          source.close();
          resolve(JSON.parse(event.data));
        });
        source.onerror = () => {
          clearTimeout(timer);
          source.close();
          reject(new Error("sse error"));
        };
      }),
  );
  assert.equal(sse.kind, "connected");
  assert.equal(sse.transport, "sse");
  assert.equal(sse.event, "fiducia:refresh");

  assert.deepEqual(pageErrors, []);
});

async function pageText(page) {
  return page.$eval("body", (body) => body.textContent ?? "");
}
