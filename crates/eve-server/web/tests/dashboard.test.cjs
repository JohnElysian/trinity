const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");

// A small DOM double lets these tests exercise the shipped script and its
// asynchronous responses without installing a browser or changing its exports.
class Element {
  constructor() {
    this.children = [];
    this.style = {};
    this.dataset = {};
    this.listeners = {};
    this.value = "";
    this.textContent = "";
    this.disabled = false;
    this.classList = { toggle() {}, add() {}, remove() {} };
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  get options() { return this.children; }
  addEventListener(name, callback) { this.listeners[name] = callback; }
  setAttribute() {}
  createTHead() { return new Element(); }
  createTBody() { return new Element(); }
  insertRow() { return new Element(); }
  insertCell() { return new Element(); }
}

function dashboard() {
  const elements = new Map();
  const element = (id) => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  };
  const action = new Element();
  const minimize = new Element();
  const fixtures = {
    "/api/summary": { players: 1, bots: { total: 0, online: 0, inSpace: 0, running: true }, corporations: 0, alliances: 0, wars: 0, lostInFights: 0, systemsHeld: 0, uptimeSeconds: 1 },
    "/api/players": [{ name: "Test Pilot", ticker: null, alliance: null, ship: null }],
    "/api/killboard": { total: 0, sides: [], ships: [] },
    "/api/market": null, "/api/relations": null, "/api/events": null,
    "/api/map": { regions: [], systems: [], jumps: [] },
    "/api/sov": { systems: [], alliances: [] },
  };
  const requests = [];
  const context = vm.createContext({
    document: {
      getElementById: element,
      createElement: () => new Element(),
      createElementNS: () => new Element(),
      querySelectorAll: (selector) => selector === "#w-control button" ? [action, minimize]
        : selector === "#w-control .body button, [data-control]" ? [action]
        : [],
    },
    Option: class extends Element {
      constructor(label, value) { super(); this.textContent = label; this.value = value; }
    },
    fetch: async (url) => {
      requests.push(url);
      const data = typeof fixtures[url] === "function" ? await fixtures[url]() : fixtures[url] ?? (url in fixtures ? null : []);
      return { ok: true, json: async () => data };
    },
    AbortController, Error, Intl, setTimeout, clearTimeout, setInterval: () => {},
    window: { setTimeout, clearTimeout },
  });
  let source = fs.readFileSync(path.join(__dirname, "../dist/app.js"), "utf8");
  source = source.replace(/export \{\};?/, "").replace(/\ntick\(\);[\s\S]*$/, "");
  vm.runInContext(source + "\nglobalThis.api = {refresh, refreshBots, searchPilots, lists, bounds};", context);
  return { ...context.api, element, fixtures, requests, action, minimize };
}

test("a failed panel leaves other panels refreshing and preserves the previous rows", async () => {
  const d = dashboard();
  await d.refresh();
  d.fixtures["/api/players"] = () => { throw new Error("players offline"); };
  d.fixtures["/api/killboard"] = { total: 7, sides: [], ships: [] };
  await d.refresh();
  assert.equal(d.lists.get("players").items[0].name, "Test Pilot");
  assert.match(d.element("killboard-total").textContent, /^7 ships/);
  assert.match(d.element("statusline").textContent, /players offline/);
});

test("a second poll cannot race an outstanding poll or duplicate the map load", async () => {
  const d = dashboard();
  let release;
  const summary = d.fixtures["/api/summary"];
  d.fixtures["/api/summary"] = () => new Promise((resolve) => { release = () => resolve(summary); });
  const first = d.refresh();
  await d.refresh();
  assert.equal(d.requests.filter((url) => url === "/api/summary").length, 1);
  release();
  await first;
  assert.equal(d.requests.filter((url) => url === "/api/map").length, 1);
});

test("out of order searches cannot replace newer results or repopulate a cleared search", async () => {
  const d = dashboard();
  let old;
  d.fixtures["/api/pilots?q=old"] = () => new Promise((resolve) => { old = resolve; });
  d.fixtures["/api/pilots?q=new"] = [{ id: 2, name: "New Pilot" }];
  d.element("pilot-search").value = "old";
  const first = d.searchPilots();
  d.element("pilot-search").value = "new";
  await d.searchPilots();
  old([{ id: 1, name: "Old Pilot" }]);
  await first;
  assert.equal(d.element("pilot-results").children[0].textContent, "New Pilot");
  d.element("pilot-search").value = "";
  await d.searchPilots();
  assert.equal(d.element("pilot-results").children.length, 0);
});

test("search failures are shown in the search results", async () => {
  const d = dashboard();
  d.fixtures["/api/pilots?q=bad"] = () => { throw new Error("offline"); };
  d.element("pilot-search").value = "bad";
  await d.searchPilots();
  assert.match(d.element("pilot-results").children[0].textContent, /Search unavailable.*offline/);
});

test("disabling simulation controls keeps window minimize available", async () => {
  const d = dashboard();
  d.fixtures["/api/events"] = { controls: false, raids: [], said: [], log: [] };
  await Promise.all(d.refreshBots());
  assert.equal(d.action.disabled, true);
  assert.equal(d.minimize.disabled, false);
});

test("the hub selector follows replacement hubs even when their count stays the same", async () => {
  const d = dashboard();
  const market = (station) => ({ hubs: [{ station, goods: [] }], shortages: [] });
  d.fixtures["/api/market"] = market("Old Hub");
  await Promise.all(d.refreshBots());
  d.fixtures["/api/market"] = market("New Hub");
  await Promise.all(d.refreshBots());
  assert.equal(d.element("hub").value, "New Hub");
  assert.equal(d.element("hub").options[0].textContent, "New Hub");
});

test("an empty map still has a finite nonzero viewport", () => {
  const d = dashboard();
  const box = d.bounds([], 2);
  assert.ok(Object.values(box).every(Number.isFinite));
  assert.ok(box.w > 0 && box.h > 0);
});
