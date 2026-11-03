// The dashboard: polls the server's JSON and fills the windows in, in the
// manner of the EVE client: overview-style lists sorted by any column, the
// game's security colours, [CORP] and <ALLIANCE> tickers, EVE time.

interface Summary {
  players: number;
  bots: { total: number; online: number; inSpace: number; running: boolean };
  corporations: number;
  alliances: number;
  wars: number;
  lostInFights: number;
  systemsHeld: number;
  uptimeSeconds: number;
}

interface War {
  declaredBy: string;
  declaredByTicker: string;
  against: string;
  againstTicker: string;
  declared: number | null;
  ends: number | null;
  mutual: boolean;
  bots: boolean;
  losses: number;
}

interface Player {
  name: string;
  ticker: string | null;
  alliance: string | null;
  ship: string | null;
  system?: string;
  systemSecurity?: number;
  docked?: boolean;
}

interface SystemRow {
  name: string;
  security: number;
  region: string;
  inSpace: number;
  docked: number;
}

interface AllianceRow {
  name: string;
  ticker: string;
  corporations: number;
  pilots: number;
  kind: string;
  region: string | null;
  systems: number;
}

interface SiegeRow {
  attacker: string;
  defender: string;
  system: string;
  region: string;
  security: number;
  starts: number;
  fighting: boolean;
  attackers: number | null;
  defenders: number | null;
}

interface NewsItem {
  time: number;
  text: string;
}

interface CorporationRow {
  name: string;
  ticker: string;
  members: number;
  alliance: string | null;
  bots: boolean;
}

interface Kill {
  time: number | null;
  victim: string | null;
  victimCorporation: string | null;
  ship: string | null;
  system: string | null;
  finalBlow: string | null;
  pod: boolean;
}

interface Killboard {
  total: number;
  sides: { name: string; ticker: string; kills: number; losses: number; bots: boolean }[];
  ships: { ship: string; lost: number }[];
}

interface MarketLine {
  name: string;
  kind: string;
  stock: number;
  ask: number;
  bid: number;
  npcAsk: number;
}

interface Market {
  minedM3: number;
  refinedUnits: number;
  hauledM3: number;
  hullsBuilt: number;
  hullsBought: number;
  hullsFromNpcs: number;
  soldToPlayers: number;
  boughtFromPlayers: number;
  playerOrdersFilled: number;
  orders: number;
  corpTax: number;
  hubs: { station: string; worth: number; goods: MarketLine[] }[];
  shortages: { name: string; wanted: number }[];
}

interface Relations {
  standings: { side: string; player: string; heat: number; favor: number; flags: string[] }[];
  wingmen: { bot: string; player: string; rate: number }[];
  hunting: number;
  grudges: number;
}

interface Followed {
  id: number;
  name: string;
  role: string;
  corporation: string;
  alliance: string | null;
  ship: string;
  tier: number;
  isk: number;
  online: boolean;
  activity: string;
  system: string;
  security: number;
  station: string | null;
  history: NewsItem[];
}

interface Events {
  raids: { system: string; raiders: number; kills: number; lost: number }[];
  log: NewsItem[];
  said: NewsItem[];
  controls: boolean;
}

/// One column of a list: its heading, what it sorts by, and its cell.
interface Column<T> {
  title: string;
  num?: boolean;
  key: (item: T) => string | number;
  cell: (item: T) => (Node | string)[];
}

// A module, so nothing here is mistaken for the window's own globals.
export {};

const REFRESH_MS = 15_000;
const numbers = new Intl.NumberFormat("en-US");

async function get<T>(path: string): Promise<T> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 10_000);
  try {
    const response = await fetch(path, { cache: "no-store", signal: controller.signal });
    if (!response.ok) throw new Error(`${path}: ${response.status}`);
    return (await response.json()) as T;
  } finally {
    clearTimeout(timeout);
  }
}

function span(text: string, className: string): HTMLElement {
  const e = document.createElement("span");
  e.textContent = text;
  e.className = className;
  return e;
}

const corpTicker = (t: string | null | undefined) => span(t ? ` [${t}]` : "", "tick");
const allianceTicker = (t: string | null | undefined) => span(t ? ` <${t}>` : "", "tick");

/// The security status colours of the client's map and overview, 1.0 to 0.0.
const SECURITY_COLOURS = ["#f00000", "#d73000", "#f04800", "#f06000", "#d77700", "#efef00", "#8fef2f", "#00f000", "#00ef47", "#48f0c0", "#2fefef"];

function security(s: number): HTMLElement {
  const rounded = Math.max(-1, Math.min(1, Math.round(s * 10) / 10));
  const e = span(rounded.toFixed(1), "sec");
  e.style.color = SECURITY_COLOURS[Math.max(0, Math.round(rounded * 10))];
  return e;
}

const pad = (n: number) => String(n).padStart(2, "0");

/// EVE time is UTC, written YYYY.MM.DD HH:MM.
function eveTime(ms: number | null): string {
  if (!ms) {
    return "";
  }
  const d = new Date(ms);
  return `${d.getUTCFullYear()}.${pad(d.getUTCMonth() + 1)}.${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
}

// ---- lists ------------------------------------------------------------------------

interface List {
  columns: Column<any>[];
  items: any[];
  sortBy: number;
  descending: boolean;
  empty: string;
}

const lists = new Map<string, List>();

/// Shows a list, sorted as its reader last chose (or by its first column).
function list<T>(id: string, columns: Column<T>[], items: T[], empty: string, sortBy = 0, descending = false): void {
  const known = lists.get(id);
  lists.set(id, {
    columns,
    items,
    empty,
    sortBy: known?.sortBy ?? sortBy,
    descending: known?.descending ?? descending,
  });
  draw(id);
}

function draw(id: string): void {
  const l = lists.get(id);
  const table = document.getElementById(id) as HTMLTableElement | null;
  if (!l || !table) {
    return;
  }
  const column = l.columns[l.sortBy];
  const items = [...l.items].sort((a, b) => {
    const x = column.key(a);
    const y = column.key(b);
    const order = typeof x === "number" && typeof y === "number" ? x - y : String(x).localeCompare(String(y));
    return l.descending ? -order : order;
  });
  table.replaceChildren();
  const head = table.createTHead().insertRow();
  l.columns.forEach((c, i) => {
    const th = document.createElement("th");
    th.textContent = c.title;
    th.className = [c.num ? "num" : "", i === l.sortBy ? "sorted" : "", i === l.sortBy && !l.descending ? "up" : ""].join(" ").trim();
    th.addEventListener("click", () => {
      l.descending = l.sortBy === i ? !l.descending : !!c.num;
      l.sortBy = i;
      draw(id);
    });
    head.append(th);
  });
  const body = table.createTBody();
  if (items.length === 0) {
    const td = body.insertRow().insertCell();
    td.colSpan = l.columns.length;
    td.className = "empty";
    td.textContent = l.empty;
    return;
  }
  for (const item of items) {
    const tr = body.insertRow();
    for (const c of l.columns) {
      const td = tr.insertCell();
      if (c.num) {
        td.className = "num";
      }
      td.append(...c.cell(item));
    }
  }
}

const count = (n: number) => [numbers.format(n)];

const compact = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });
const isk = (n: number) => `${compact.format(n)} ISK`;
const price = (n: number) => (n >= 1000 ? compact.format(n) : n.toFixed(2));

// ---- windows ----------------------------------------------------------------------

function showStatus(s: Summary): void {
  const rows: [string, string][] = [
    ["Pilots online", numbers.format(s.players)],
    ["Capsuleers simulated", numbers.format(s.bots.total)],
    ["Simulated online", numbers.format(s.bots.online)],
    ["Simulated in space", numbers.format(s.bots.inSpace)],
    ["Corporations", numbers.format(s.corporations)],
    ["Alliances", numbers.format(s.alliances)],
    ["Wars under way", numbers.format(s.wars)],
    ["Ships lost in war", numbers.format(s.lostInFights)],
    ["Sovereign systems", numbers.format(s.systemsHeld)],
    ["Server up", uptime(s.uptimeSeconds)],
  ];
  const dl = document.getElementById("status") as HTMLElement;
  dl.replaceChildren(
    ...rows.flatMap(([label, value]) => {
      const dt = document.createElement("dt");
      dt.textContent = label;
      const dd = document.createElement("dd");
      dd.textContent = value;
      return [dt, dd];
    }),
  );
  const online = s.players + s.bots.online;
  const line = document.getElementById("statusline") as HTMLElement;
  line.textContent = `Status: OK, ${numbers.format(online)} players${s.bots.running ? "" : " (simulation paused)"}`;
}

function uptime(seconds: number): string {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  return d > 0 ? `${d}d ${h}h ${m}m` : h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/// Refresh healthy panels even if another endpoint fails. Keep their last good
/// contents on failure, and report which requests failed in the status line.
async function updatePanel<T>(path: string, render: (value: T) => void | Promise<void>): Promise<string | null> {
  try {
    await render(await get<T>(path));
    return null;
  } catch (e) {
    return e instanceof Error ? e.message : String(e);
  }
}

let refreshing = false;

async function refresh(): Promise<void> {
  if (refreshing) return;
  refreshing = true;
  try {
    const failures = (await Promise.all([
      updatePanel<Summary>("/api/summary", async (summary) => {
        showStatus(summary);
      }),
      updatePanel<War[]>("/api/wars", async (wars) => {
        list<War>("wars", [
          { title: "Aggressor", key: (w) => w.declaredBy, cell: (w) => [w.declaredBy, allianceTicker(w.declaredByTicker)] },
          { title: "", key: () => "", cell: () => [span("vs", "hostile")] },
          { title: "Defender", key: (w) => w.against, cell: (w) => [w.against, allianceTicker(w.againstTicker)] },
          { title: "Declared", key: (w) => w.declared ?? 0, cell: (w) => [eveTime(w.declared)] },
          { title: "Ends", key: (w) => w.ends ?? 0, cell: (w) => [eveTime(w.ends)] },
          { title: "Losses", num: true, key: (w) => w.losses, cell: (w) => count(w.losses) },
          { title: "Type", key: (w) => (w.mutual ? 1 : 0), cell: (w) => [span(w.bots ? "mutual (simulated)" : w.mutual ? "mutual" : "declared", "dim")] },
        ], wars, "There are no wars.", 3, true);
      }),
      updatePanel<Player[]>("/api/players", async (players) => {
        list<Player>("players", [
          { title: "Name", key: (p) => p.name, cell: (p) => [p.name] },
          { title: "Corporation", key: (p) => p.ticker ?? "", cell: (p) => [corpTicker(p.ticker), p.alliance ? span(` ${p.alliance}`, "dim") : ""] },
          { title: "Location", key: (p) => p.system ?? "", cell: (p) => [security(p.systemSecurity ?? 0), ` ${p.system ?? ""}`, p.docked ? span(" (docked)", "dim") : ""] },
          { title: "Ship", key: (p) => p.ship ?? "", cell: (p) => [p.ship ?? ""] },
        ], players, "No pilots are logged in.");
      }),
      updatePanel<SystemRow[]>("/api/systems", async (systems) => {
        list<SystemRow>("systems", [
          { title: "System", key: (s) => s.name, cell: (s) => [security(s.security), ` ${s.name}`] },
          { title: "Region", key: (s) => s.region, cell: (s) => [span(s.region, "dim")] },
          { title: "In space", num: true, key: (s) => s.inSpace, cell: (s) => count(s.inSpace) },
          { title: "Docked", num: true, key: (s) => s.docked, cell: (s) => count(s.docked) },
          { title: "Total", num: true, key: (s) => s.inSpace + s.docked, cell: (s) => count(s.inSpace + s.docked) },
        ], systems, "No one is about.", 4, true);
      }),
      updatePanel<AllianceRow[]>("/api/alliances", async (alliances) => {
        list<AllianceRow>("alliances", [
          { title: "Alliance", key: (a) => a.name, cell: (a) => [a.name, allianceTicker(a.ticker)] },
          { title: "Holds", key: (a) => a.region ?? "", cell: (a) => [a.region ?? span(a.kind === "players" ? "" : a.kind, "dim")] },
          { title: "Corps", num: true, key: (a) => a.corporations, cell: (a) => count(a.corporations) },
          { title: "Pilots", num: true, key: (a) => a.pilots, cell: (a) => count(a.pilots) },
          { title: "Systems", num: true, key: (a) => a.systems, cell: (a) => (a.systems > 0 ? count(a.systems) : [span("-", "dim")]) },
        ], alliances, "No alliances have been founded.", 3, true);
      }),
      updatePanel<CorporationRow[]>("/api/corporations", async (corporations) => {
        list<CorporationRow>("corporations", [
          { title: "Corporation", key: (c) => c.name, cell: (c) => [c.name, corpTicker(c.ticker), c.bots ? "" : span(" (players)", "dim")] },
          { title: "Alliance", key: (c) => c.alliance ?? "", cell: (c) => [allianceTicker(c.alliance)] },
          { title: "Members", num: true, key: (c) => c.members, cell: (c) => count(c.members) },
        ], corporations, "No corporations have been founded.", 2, true);
      }),
      updatePanel<SiegeRow[]>("/api/sieges", async (sieges) => {
        list<SiegeRow>("sieges", [
          { title: "System", key: (s) => s.system, cell: (s) => [security(s.security), ` ${s.system}`, span(` ${s.region}`, "dim")] },
          { title: "Attacker", key: (s) => s.attacker, cell: (s) => [s.attacker] },
          { title: "Defender", key: (s) => s.defender, cell: (s) => [s.defender] },
          {
            title: "When",
            key: (s) => s.starts,
            cell: (s) =>
              s.fighting
                ? [span(`BATTLE ${s.attackers ?? 0} vs ${s.defenders ?? 0}`, "hostile")]
                : [eveTime(s.starts)],
          },
        ], sieges, "No sieges are planned.", 3, false);
        await refreshMap(sieges);
      }),
      updatePanel<NewsItem[]>("/api/news", async (news) => {
        list<NewsItem>("news", [
          { title: "Time", key: (n) => n.time, cell: (n) => [eveTime(n.time)] },
          { title: "Report", key: (n) => n.text, cell: (n) => [n.text] },
        ], news, "Nothing has happened in null-sec yet.", 0, true);
      }),
      updatePanel<Kill[]>("/api/kills", async (kills) => {
        list<Kill>("kills", [
          { title: "Time", key: (k) => k.time ?? 0, cell: (k) => [eveTime(k.time)] },
          { title: "Victim", key: (k) => k.victim ?? "", cell: (k) => [k.victim ?? "", k.victimCorporation ? span(` (${k.victimCorporation})`, "dim") : ""] },
          { title: "Destroyed", key: (k) => k.ship ?? "", cell: (k) => [k.ship ?? "", k.pod ? span(" (pod)", "hostile") : ""] },
          { title: "System", key: (k) => k.system ?? "", cell: (k) => [k.system ?? ""] },
          { title: "Final blow", key: (k) => k.finalBlow ?? "", cell: (k) => [k.finalBlow ?? ""] },
        ], kills, "No kills have been reported.", 0, true);
      }),
      ...refreshBots(),
    ])).filter((failure): failure is string => failure !== null);
    if (failures.length) {
      setText("statusline", `Status: some panels unavailable, retrying (${failures.join("; ")})`);
    }
  } finally {
    refreshing = false;
  }
}


// ---- the sovereignty map -----------------------------------------------------------
//
// New Eden from above, every system a dot in its holder's colour; or one region
// laid out as dotlan drew them, boxes named and ticked, the gates between,
// and the systems next door in the regions around it.

interface MapData {
  regions: [number, string][];
  /// [id, name, region, constellation, x, z, security], x and z in light years.
  systems: [number, string, number, number, number, number, number][];
  jumps: [number, number][];
}

interface SovData {
  systems: [number, number][];
  alliances: [number, string, string][];
}

interface Star {
  id: number;
  name: string;
  region: number;
  constellation: number;
  x: number;
  y: number;
  sec: number;
  links: number[];
}

interface Atlas {
  stars: Map<number, Star>;
  regions: Map<number, string>;
  byName: Map<string, number>;
  regionStars: Map<number, Star[]>;
  jumps: [number, number][];
}

/// What shows a system on the map: what is filled in its holder's colour,
/// and, in a region, its box and the ticker under its name.
interface Mark {
  fill: SVGElement;
  box: SVGElement;
  ticker: SVGTSpanElement | null;
}

interface ViewBox {
  x: number;
  y: number;
  w: number;
  h: number;
}

const SVG_NS = "http://www.w3.org/2000/svg";
/// A system's box in a region, and the room kept around it.
const BOX_W = 66;
const BOX_H = 26;
const BOX_GAP = 10;

let atlas: Atlas | null = null;
let holders = new Map<number, number>();
let allianceNames = new Map<number, { name: string; ticker: string }>();
let besieged = new Set<number>();
/// The region shown; null for all of New Eden.
let mapRegion: number | null = null;
/// The alliance picked out in the legend.
let pickedAlliance: number | null = null;
const marks = new Map<number, Mark>();
let homeView: ViewBox = { x: 0, y: 0, w: 1, h: 1 };
let view: ViewBox = homeView;

const mapSvg = () => document.getElementById("map") as unknown as SVGSVGElement;

function svgEl<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}): SVGElementTagNameMap[K] {
  const e = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) {
    e.setAttribute(k, String(v));
  }
  return e;
}

function buildAtlas(data: MapData): Atlas {
  const stars = new Map<number, Star>();
  const byName = new Map<string, number>();
  for (const [id, name, region, constellation, x, z, sec] of data.systems) {
    // Seen from above, north up: x to the right, z up the page.
    stars.set(id, { id, name, region, constellation, x, y: -z, sec, links: [] });
    byName.set(name, id);
  }
  for (const [a, b] of data.jumps) {
    stars.get(a)?.links.push(b);
    stars.get(b)?.links.push(a);
  }
  const regionStars = new Map<number, Star[]>();
  for (const s of stars.values()) {
    // Regions no gate reaches (Jove space) stay off the map.
    if (s.links.length > 0) {
      const list = regionStars.get(s.region) ?? [];
      list.push(s);
      regionStars.set(s.region, list);
    }
  }
  return { stars, regions: new Map(data.regions), byName, regionStars, jumps: data.jumps };
}

/// An alliance's colour, the same on every visit: its ID spread round the hue
/// circle, in one of three lightnesses so neighbours tell apart.
function allianceColour(id: number, alpha = 1): string {
  const hue = (Math.imul(id, 2654435761) >>> 0) % 360;
  const light = 48 + ((Math.imul(id, 40503) >>> 0) % 3) * 9;
  return `hsla(${hue}, 72%, ${light}%, ${alpha})`;
}

/// An unclaimed system's colour: empire space greyed blue, low-sec brown,
/// null-sec dark.
function unclaimedColour(s: Star): string {
  if (s.sec >= 0.45) {
    return mapRegion === null ? "#4a5566" : "rgba(90, 110, 140, .18)";
  }
  if (s.sec > 0) {
    return mapRegion === null ? "#5e5040" : "rgba(140, 110, 70, .16)";
  }
  return mapRegion === null ? "#33363d" : "rgba(255, 255, 255, .04)";
}

function securityColour(s: number): string {
  const rounded = Math.max(0, Math.min(1, Math.round(s * 10) / 10));
  return SECURITY_COLOURS[Math.round(rounded * 10)];
}

async function refreshMap(sieges: SiegeRow[]): Promise<void> {
  try {
    if (!atlas) {
      atlas = buildAtlas(await get<MapData>("/api/map"));
      const select = document.getElementById("map-region") as HTMLSelectElement;
      const names = [...atlas.regionStars.keys()]
        .map((id) => [id, atlas?.regions.get(id) ?? String(id)] as const)
        .sort((a, b) => a[1].localeCompare(b[1]));
      select.append(...names.map(([id, name]) => new Option(name, String(id))));
      drawMap();
    }
    const sov = await get<SovData>("/api/sov");
    holders = new Map(sov.systems);
    allianceNames = new Map(sov.alliances.map(([id, name, ticker]) => [id, { name, ticker }]));
    if (pickedAlliance !== null && !allianceNames.has(pickedAlliance)) {
      pickedAlliance = null;
    }
    const known = atlas;
    besieged = new Set(sieges.map((s) => known.byName.get(s.system)).filter((id): id is number => id !== undefined));
    paintMap();
  } catch (e) {
    setText("map-summary", `The map did not load (${e instanceof Error ? e.message : String(e)})`);
  }
}

function drawMap(): void {
  if (!atlas) {
    return;
  }
  marks.clear();
  mapSvg().replaceChildren();
  if (mapRegion === null) {
    drawGalaxy(atlas);
  } else {
    drawRegion(atlas, mapRegion);
  }
  setView(homeView);
  paintMap();
}

function bounds(points: { x: number; y: number }[], pad: number): ViewBox {
  const xs = points.map((p) => p.x);
  const ys = points.map((p) => p.y);
  if (!points.length) return { x: -pad, y: -pad, w: pad * 2 || 1, h: pad * 2 || 1 };
  const x = Math.min(...xs) - pad;
  const y = Math.min(...ys) - pad;
  return { x, y, w: Math.max(...xs) + pad - x, h: Math.max(...ys) + pad - y };
}

function drawGalaxy(a: Atlas): void {
  const svg = mapSvg();
  const stars = [...a.regionStars.values()].flat();
  const box = bounds(stars, 2);
  const span = Math.max(box.w, box.h);
  const r = span / 420;
  // The gates: within a region, and between regions.
  let within = "";
  let between = "";
  for (const [from, to] of a.jumps) {
    const p = a.stars.get(from);
    const q = a.stars.get(to);
    if (!p || !q) {
      continue;
    }
    const d = `M${p.x} ${p.y}L${q.x} ${q.y}`;
    if (p.region === q.region) {
      within += d;
    } else {
      between += d;
    }
  }
  svg.append(
    svgEl("path", { class: "jumps", d: within, "stroke-width": r * 0.3 }),
    svgEl("path", { class: "jumps region", d: between, "stroke-width": r * 0.3, "stroke-dasharray": `${r * 2} ${r * 1.5}` }),
  );
  const dots = svgEl("g");
  for (const s of stars) {
    const c = svgEl("circle", { class: "star", cx: s.x, cy: s.y, r, "stroke-width": r * 0.9, "data-id": s.id });
    dots.append(c);
    marks.set(s.id, { fill: c, box: c, ticker: null });
  }
  svg.append(dots);
  const labels = svgEl("g");
  for (const [id, list] of a.regionStars) {
    const x = list.reduce((t, s) => t + s.x, 0) / list.length;
    const y = list.reduce((t, s) => t + s.y, 0) / list.length;
    const t = svgEl("text", { class: "region-label", x, y, "font-size": span / 85, "stroke-width": span / 450, "data-region": id });
    t.textContent = a.regions.get(id) ?? "";
    labels.append(t);
  }
  svg.append(labels);
  homeView = box;
}

interface LayoutNode {
  star: Star;
  outside: boolean;
  x: number;
  y: number;
  hx: number;
  hy: number;
}

/// Spreads the boxes so none overlap, each pulled gently back to where the
/// stars put it.
function relax(nodes: LayoutNode[]): void {
  const w = BOX_W + BOX_GAP;
  const h = BOX_H + BOX_GAP;
  for (let round = 0; round < 300; round++) {
    let moved = false;
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        const p = nodes[i];
        const q = nodes[j];
        const dx = q.x - p.x;
        const dy = q.y - p.y;
        const ox = w - Math.abs(dx);
        const oy = h - Math.abs(dy);
        if (ox <= 0 || oy <= 0) {
          continue;
        }
        moved = true;
        if (ox / w < oy / h) {
          const push = ((dx >= 0 ? 1 : -1) * ox) / 2;
          p.x -= push;
          q.x += push;
        } else {
          const push = ((dy >= 0 ? 1 : -1) * oy) / 2;
          p.y -= push;
          q.y += push;
        }
      }
    }
    for (const n of nodes) {
      n.x += (n.hx - n.x) * 0.02;
      n.y += (n.hy - n.y) * 0.02;
    }
    if (!moved && round > 20) {
      break;
    }
  }
}

function drawRegion(a: Atlas, region: number): void {
  const svg = mapSvg();
  const inside = a.regionStars.get(region) ?? [];
  const insideIds = new Set(inside.map((s) => s.id));
  const outside = new Map<number, Star>();
  for (const s of inside) {
    for (const l of s.links) {
      const n = a.stars.get(l);
      if (n && !insideIds.has(l)) {
        outside.set(l, n);
      }
    }
  }
  // Scaled so that a system's nearest neighbour is typically a box and a half away.
  const nearest = inside
    .map((s) => Math.min(...inside.filter((o) => o !== s).map((o) => Math.hypot(o.x - s.x, o.y - s.y))))
    .filter((d) => Number.isFinite(d) && d > 0)
    .sort((x, y) => x - y);
  const scale = nearest.length ? (BOX_W * 1.5) / nearest[Math.floor(nearest.length / 2)] : 100;
  const nodes: LayoutNode[] = [...inside, ...outside.values()].map((s) => {
    const x = s.x * scale;
    const y = s.y * scale;
    return { star: s, outside: !insideIds.has(s.id), x, y, hx: x, hy: y };
  });
  relax(nodes);
  const at = new Map(nodes.map((n) => [n.star.id, n]));

  let plain = "";
  let constellation = "";
  let regional = "";
  for (const n of nodes) {
    for (const l of n.star.links) {
      const m = at.get(l);
      // Each gate once, and none between two systems of other regions.
      if (!m || l < n.star.id || (n.outside && m.outside)) {
        continue;
      }
      const d = `M${n.x} ${n.y}L${m.x} ${m.y}`;
      if (n.star.region !== m.star.region) {
        regional += d;
      } else if (n.star.constellation !== m.star.constellation) {
        constellation += d;
      } else {
        plain += d;
      }
    }
  }
  svg.append(
    svgEl("path", { class: "jumps", d: plain, "stroke-width": 1.2 }),
    svgEl("path", { class: "jumps constellation", d: constellation, "stroke-width": 1.2 }),
    svgEl("path", { class: "jumps region", d: regional, "stroke-width": 1.2 }),
  );

  for (const n of nodes) {
    const g = svgEl("g", { class: n.outside ? "sys outside" : "sys", transform: `translate(${n.x} ${n.y})`, "data-id": n.star.id });
    const rect = svgEl("rect", { x: -BOX_W / 2, y: -BOX_H / 2, width: BOX_W, height: BOX_H, rx: 3 });
    const name = svgEl("text", { class: "name", y: -2 });
    name.textContent = n.star.name;
    const line2 = svgEl("text", { class: "line2", y: 8 });
    if (n.outside) {
      line2.textContent = a.regions.get(n.star.region) ?? "";
      g.append(rect, name, line2);
      g.setAttribute("data-region", String(n.star.region));
      marks.set(n.star.id, { fill: rect, box: g, ticker: null });
    } else {
      const sec = svgEl("tspan", { fill: securityColour(n.star.sec) });
      sec.textContent = n.star.sec.toFixed(1);
      const ticker = svgEl("tspan", { class: "ticker" });
      line2.append(sec, ticker);
      g.append(rect, name, line2);
      marks.set(n.star.id, { fill: rect, box: g, ticker });
    }
    svg.append(g);
  }
  homeView = bounds(nodes, BOX_W);
}

/// Colours the systems by who holds them, marks the sieges, and lists the
/// holders alongside.
function paintMap(): void {
  if (!atlas) {
    return;
  }
  const galaxy = mapRegion === null;
  for (const [id, m] of marks) {
    const star = atlas.stars.get(id);
    if (!star) {
      continue;
    }
    const holder = holders.get(id);
    m.fill.style.fill = holder !== undefined ? allianceColour(holder, galaxy ? 1 : 0.55) : unclaimedColour(star);
    m.fill.classList.toggle("siege", besieged.has(id));
    m.box.classList.toggle("faded", pickedAlliance !== null && holder !== pickedAlliance);
    if (m.ticker) {
      const t = holder !== undefined ? allianceNames.get(holder)?.ticker : undefined;
      m.ticker.textContent = t ? `  ${t}` : "";
    }
  }

  // The holders of what is shown: everything, or the region's own systems.
  const shown = galaxy ? [...atlas.regionStars.values()].flat() : atlas.regionStars.get(mapRegion ?? 0) ?? [];
  const held = new Map<number, number>();
  for (const s of shown) {
    const h = holders.get(s.id);
    if (h !== undefined) {
      held.set(h, (held.get(h) ?? 0) + 1);
    }
  }
  const claimed = [...held.values()].reduce((t, n) => t + n, 0);
  setText(
    "map-summary",
    galaxy
      ? `${numbers.format(claimed)} systems held by ${numbers.format(held.size)} alliances`
      : `${atlas.regions.get(mapRegion ?? 0) ?? ""}: ${numbers.format(claimed)} of ${numbers.format(shown.length)} systems claimed`,
  );
  const legend = document.getElementById("map-legend") as HTMLOListElement;
  const rows = [...held.entries()].sort((x, y) => y[1] - x[1] || x[0] - y[0]);
  legend.replaceChildren(
    ...rows.map(([id, n]) => {
      const info = allianceNames.get(id);
      const li = document.createElement("li");
      const b = document.createElement("button");
      b.type = "button";
      b.className = id === pickedAlliance ? "on" : "";
      b.title = info ? `${info.name} <${info.ticker}>` : String(id);
      const swatch = span("", "swatch");
      swatch.style.background = allianceColour(id);
      b.append(swatch, span(info?.name ?? String(id), "name"), allianceTicker(info?.ticker), span(numbers.format(n), "count"));
      b.addEventListener("click", () => {
        pickedAlliance = pickedAlliance === id ? null : id;
        paintMap();
      });
      li.append(b);
      return li;
    }),
  );
  if (rows.length === 0) {
    const li = document.createElement("li");
    li.append(span(galaxy ? "Nobody holds any space yet." : "Nobody holds space here.", "dim"));
    legend.append(li);
  }
}

function showRegion(region: number | null): void {
  mapRegion = region;
  (document.getElementById("map-region") as HTMLSelectElement).value = region === null ? "" : String(region);
  drawMap();
}

function describe(id: number): string {
  const s = atlas?.stars.get(id);
  if (!s) {
    return "";
  }
  const holder = holders.get(id);
  const info = holder !== undefined ? allianceNames.get(holder) : undefined;
  const who = info ? `held by ${info.name} <${info.ticker}>` : "unclaimed";
  const siege = besieged.has(id) ? " — under siege" : "";
  return `${s.name} (${s.sec.toFixed(1)}), ${atlas?.regions.get(s.region) ?? ""}: ${who}${siege}`;
}

// Zooming and panning move the view box.

function setView(v: ViewBox): void {
  view = v;
  mapSvg().setAttribute("viewBox", `${v.x} ${v.y} ${v.w} ${v.h}`);
}

/// A point on the screen in the map's own units.
function mapPoint(clientX: number, clientY: number): DOMPoint {
  const svg = mapSvg();
  const m = svg.getScreenCTM();
  const p = new DOMPoint(clientX, clientY);
  return m ? p.matrixTransform(m.inverse()) : p;
}

{
  const svg = mapSvg();
  let drag: { x: number; y: number; from: ViewBox; moved: boolean } | null = null;
  let dragged = false;

  svg.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      const p = mapPoint(e.clientX, e.clientY);
      const k = e.deltaY < 0 ? 0.8 : 1.25;
      const w = Math.min(homeView.w * 2, Math.max(homeView.w / 60, view.w * k));
      const f = w / view.w;
      setView({ x: p.x - (p.x - view.x) * f, y: p.y - (p.y - view.y) * f, w, h: view.h * f });
    },
    { passive: false },
  );
  svg.addEventListener("pointerdown", (e) => {
    drag = { x: e.clientX, y: e.clientY, from: view, moved: false };
    dragged = false;
  });
  svg.addEventListener("pointermove", (e) => {
    if (drag) {
      const dx = e.clientX - drag.x;
      const dy = e.clientY - drag.y;
      if (!drag.moved && Math.hypot(dx, dy) > 4) {
        drag.moved = true;
        svg.setPointerCapture(e.pointerId);
        svg.classList.add("dragging");
      }
      if (drag.moved) {
        const units = svg.getScreenCTM()?.a ?? 1;
        setView({ ...drag.from, x: drag.from.x - dx / units, y: drag.from.y - dy / units });
      }
      return;
    }
    const target = (e.target as Element).closest("[data-id]");
    const region = (e.target as Element).closest("[data-region]:not([data-id])");
    setText(
      "map-hover",
      target
        ? describe(Number(target.getAttribute("data-id")))
        : region
          ? `${atlas?.regions.get(Number(region.getAttribute("data-region"))) ?? ""}: click to open`
          : "Scroll to zoom, drag to pan. Click a system to open its region; click an alliance to pick out its space.",
    );
  });
  const endDrag = (e: PointerEvent) => {
    if (drag?.moved) {
      dragged = true;
      svg.releasePointerCapture(e.pointerId);
    }
    svg.classList.remove("dragging");
    drag = null;
  };
  svg.addEventListener("pointerup", endDrag);
  svg.addEventListener("pointercancel", endDrag);
  svg.addEventListener("click", (e) => {
    if (dragged) {
      dragged = false;
      return;
    }
    const el = e.target as Element;
    const region = el.closest("[data-region]");
    if (region) {
      showRegion(Number(region.getAttribute("data-region")));
      return;
    }
    const sys = el.closest("[data-id]");
    const star = sys ? atlas?.stars.get(Number(sys.getAttribute("data-id"))) : undefined;
    if (star && mapRegion === null) {
      showRegion(star.region);
    }
  });
  svg.addEventListener("dblclick", () => setView(homeView));
  (document.getElementById("map-region") as HTMLSelectElement).addEventListener("change", (e) => {
    const v = (e.target as HTMLSelectElement).value;
    showRegion(v === "" ? null : Number(v));
  });
  (document.getElementById("map-reset") as HTMLButtonElement).addEventListener("click", () => setView(homeView));
}

// ---- the bots' windows -------------------------------------------------------------

let market: Market | null = null;
let chosenPilot: number | null = null;
let controlsEnabled = false;
let followedNow: Followed[] = [];

function refreshBots(): Promise<string | null>[] {
  return [
    updatePanel<Killboard>("/api/killboard", (board) => {
      setText("killboard-total", `${numbers.format(board.total)} ships destroyed in the last day`);
      list("killboard", [
        { title: "Alliance / corporation", key: (k) => k.name, cell: (k) => [k.name, allianceTicker(k.ticker), k.bots ? "" : span(" (players)", "dim")] },
        { title: "Kills", num: true, key: (k) => k.kills, cell: (k) => count(k.kills) },
        { title: "Losses", num: true, key: (k) => k.losses, cell: (k) => count(k.losses) },
      ], board.sides, "Nothing has died today.", 1, true);
      list("killboard-ships", [
        { title: "Hull", key: (s) => s.ship, cell: (s) => [s.ship] },
        { title: "Lost", num: true, key: (s) => s.lost, cell: (s) => count(s.lost) },
      ], board.ships, "", 1, true);
    }),
    updatePanel<Market | null>("/api/market", (mkt) => {
      if (mkt && mkt.hubs) {
        market = mkt;
        const select = document.getElementById("hub") as HTMLSelectElement;
        const before = select.value;
        select.replaceChildren(...mkt.hubs.map((h) => new Option(h.station, h.station)));
        select.value = mkt.hubs.some((h) => h.station === before) ? before : (mkt.hubs[0]?.station ?? "");
        showMarket();
        setText(
          "market-summary",
          `${compact.format(mkt.minedM3)} m3 mined, ${compact.format(mkt.hauledM3)} m3 hauled, ${numbers.format(mkt.hullsBuilt)} hulls built, ` +
            `${numbers.format(mkt.hullsBought)} bought (${numbers.format(mkt.hullsFromNpcs)} from NPCs); players bought ${isk(mkt.soldToPlayers)} and sold ${isk(mkt.boughtFromPlayers)}; ` +
            `${numbers.format(mkt.orders)} orders up; ${isk(mkt.corpTax)} in corporation tax`,
        );
        setText("market-shortages", mkt.shortages.length ? `Short of: ${mkt.shortages.map((s) => `${s.name} (${s.wanted})`).join(", ")}` : "");
      }
    }),
    updatePanel<Relations | null>("/api/relations", (rel) => {
      if (rel && rel.standings) {
        setText("relations-summary", `${numbers.format(rel.grudges)} pilots hold a grudge; ${numbers.format(rel.hunting)} are out hunting`);
        list("relations", [
          { title: "Side", key: (r) => r.side, cell: (r) => [r.side] },
          { title: "Player", key: (r) => r.player, cell: (r) => [r.player] },
          { title: "Heat", num: true, key: (r) => r.heat, cell: (r) => [r.heat ? span(r.heat.toFixed(1), "hostile") : ""] },
          { title: "Favor", num: true, key: (r) => r.favor, cell: (r) => [r.favor ? r.favor.toFixed(1) : ""] },
          { title: "", key: (r) => r.flags.join(), cell: (r) => [span(r.flags.join(", "), r.flags.includes("hostile") ? "hostile" : "dim")] },
        ], rel.standings, "No bot side has an opinion of any player yet.", 2, true);
        list("wingmen", [
          { title: "Wingman", key: (w) => w.bot, cell: (w) => [w.bot] },
          { title: "Flies with", key: (w) => w.player, cell: (w) => [w.player] },
          { title: "Rate", num: true, key: (w) => w.rate, cell: (w) => [`${isk(w.rate)}/h`] },
        ], rel.wingmen, "Nobody has hired a wingman.");
      }
    }),
    updatePanel<Followed[] | null>("/api/followed", (followed) => {
      if (followed) {
        followedNow = followed;
        if (chosenPilot === null || !followed.some((f) => f.id === chosenPilot)) {
          chosenPilot = followed[0]?.id ?? null;
        }
        showFollowed();
      }
    }),
    updatePanel<Events | null>("/api/events", (events) => {
      if (events) {
        controlsEnabled = events.controls;
        const state = events.controls ? "Controls are on." : "Controls are off: set bots.dashboardControls in trinity.yml to use them.";
        setText("controls-state", state);
        for (const b of document.querySelectorAll<HTMLButtonElement>("#w-control .body button, [data-control]")) {
          b.disabled = !controlsEnabled;
        }
        list("raids", [
          { title: "Raid on", key: (r) => r.system, cell: (r) => [span(r.system, "hostile")] },
          { title: "Raiders", num: true, key: (r) => r.raiders, cell: (r) => count(r.raiders) },
          { title: "Kills", num: true, key: (r) => r.kills, cell: (r) => count(r.kills) },
          { title: "Lost", num: true, key: (r) => r.lost, cell: (r) => count(r.lost) },
        ], events.raids, "No raids under way.");
        list("events", [
          { title: "Time", key: (n) => n.time, cell: (n) => [eveTime(n.time)] },
          { title: "What happened", key: (n) => n.text, cell: (n) => [n.text] },
        ], [...events.said, ...events.log], "No upheavals yet.", 0, true);
      }
    }),
  ];
}


function setText(id: string, text: string): void {
  const e = document.getElementById(id);
  if (e) {
    e.textContent = text;
  }
}

function showMarket(): void {
  if (!market) {
    return;
  }
  const chosen = (document.getElementById("hub") as HTMLSelectElement).value;
  const hub = market.hubs.find((h) => h.station === chosen) ?? market.hubs[0];
  list<MarketLine>("market", [
    { title: "Item", key: (g) => g.name, cell: (g) => [g.name] },
    { title: "Kind", key: (g) => g.kind, cell: (g) => [span(g.kind, "dim")] },
    { title: "In stock", num: true, key: (g) => g.stock, cell: (g) => count(g.stock) },
    { title: "Bots sell at", num: true, key: (g) => g.ask, cell: (g) => [price(g.ask)] },
    { title: "Bots buy at", num: true, key: (g) => (g.kind === "ship" ? 0 : g.bid), cell: (g) => [g.kind === "ship" ? "" : price(g.bid)] },
    { title: "NPC ask", num: true, key: (g) => g.npcAsk, cell: (g) => [span(price(g.npcAsk), "dim")] },
  ], hub ? hub.goods : [], "The bots hold nothing here yet.", 2, true);
}

function showFollowed(): void {
  list<Followed>("followed", [
    { title: "Pilot", key: (f) => f.name, cell: (f) => [pilotButton(f), span(` ${f.role}`, "dim")] },
    { title: "Corporation", key: (f) => f.corporation, cell: (f) => [f.corporation, f.alliance ? span(` ${f.alliance}`, "dim") : ""] },
    { title: "Ship", key: (f) => f.ship, cell: (f) => [f.ship, span(` T${f.tier}`, "dim")] },
    { title: "Where", key: (f) => f.system, cell: (f) => [security(f.security), ` ${f.system}`, f.station ? span(" (docked)", "dim") : ""] },
    { title: "Doing", key: (f) => f.activity, cell: (f) => [f.online ? f.activity : span("offline", "dim")] },
    { title: "Wallet", num: true, key: (f) => f.isk, cell: (f) => [isk(f.isk)] },
    { title: "", key: () => "", cell: (f) => [unfollowButton(f)] },
  ], followedNow, "Nobody followed. Search for a pilot's name above.");
  const chosen = followedNow.find((f) => f.id === chosenPilot);
  const ol = document.getElementById("history") as HTMLOListElement;
  ol.replaceChildren(
    ...(chosen?.history ?? []).map((h) => {
      const li = document.createElement("li");
      const t = document.createElement("time");
      t.textContent = eveTime(h.time);
      li.append(t, h.text);
      return li;
    }),
  );
}

function pilotButton(f: Followed): HTMLElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "pick";
  b.textContent = f.name;
  b.addEventListener("click", () => {
    chosenPilot = f.id;
    showFollowed();
  });
  return b;
}

function unfollowButton(f: Followed): HTMLElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "pick";
  b.textContent = "Stop";
  b.title = "Stop following";
  b.dataset.control = "unfollow";
  b.disabled = !controlsEnabled;
  b.addEventListener("click", () => void control(`unfollow ${f.id}`));
  return b;
}

async function control(what: string): Promise<void> {
  const reply = document.getElementById("control-reply") as HTMLElement;
  if (!controlsEnabled) {
    reply.textContent = "Dashboard controls are off.";
    reply.className = "note hostile";
    return;
  }
  try {
    const response = await fetch(`/api/control?do=${encodeURIComponent(what)}`, { method: "POST" });
    const body = (await response.json()) as { ok?: string; error?: string };
    reply.textContent = body.ok ?? body.error ?? "";
    reply.className = body.ok ? "note" : "note hostile";
  } catch (e) {
    reply.textContent = `The server did not answer (${e instanceof Error ? e.message : String(e)})`;
    reply.className = "note hostile";
  }
  setTimeout(() => void refresh(), 1_500);
}

let searchTimer: number | undefined;
let searchVersion = 0;

async function searchPilots(): Promise<void> {
  const version = ++searchVersion;
  const q = (document.getElementById("pilot-search") as HTMLInputElement).value.trim();
  const out = document.getElementById("pilot-results") as HTMLElement;
  if (q.length < 2) {
    out.replaceChildren();
    return;
  }
  try {
    const found = await get<{ id: number; name: string }[]>(`/api/pilots?q=${encodeURIComponent(q)}`);
    if (version !== searchVersion) return;
    out.replaceChildren(
      ...found.map((p) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = "pick";
        b.textContent = p.name;
        b.title = "Follow";
        b.dataset.control = "follow";
        b.disabled = !controlsEnabled;
        b.addEventListener("click", () => {
          chosenPilot = p.id;
          void control(`follow ${p.id}`);
        });
        return b;
      }),
    );
    if (!found.length) out.append(span("No pilots found.", "dim"));
  } catch (e) {
    if (version !== searchVersion) return;
    out.replaceChildren(span(`Search unavailable (${e instanceof Error ? e.message : String(e)})`, "hostile"));
  }
}

(document.getElementById("hub") as HTMLSelectElement).addEventListener("change", showMarket);
(document.getElementById("pilot-search") as HTMLInputElement).addEventListener("input", () => {
  clearTimeout(searchTimer);
  // Invalidate the prior request as soon as the query changes, during debounce too.
  ++searchVersion;
  (document.getElementById("pilot-results") as HTMLElement).replaceChildren();
  searchTimer = window.setTimeout(() => void searchPilots(), 250);
});
for (const b of document.querySelectorAll<HTMLButtonElement>("#event-buttons button")) {
  b.addEventListener("click", () => void control(b.dataset.do ?? ""));
}
(document.getElementById("war-form") as HTMLFormElement).addEventListener("submit", (e) => {
  e.preventDefault();
  const f = new FormData(e.target as HTMLFormElement);
  void control(`war ${f.get("a")} | ${f.get("b")}`);
});
(document.getElementById("density-form") as HTMLFormElement).addEventListener("submit", (e) => {
  e.preventDefault();
  const f = new FormData(e.target as HTMLFormElement);
  void control(`density ${f.get("region")} ${f.get("factor")}`);
});

// ---- the neocom clock and the minimize buttons ------------------------------------

function tick(): void {
  const now = new Date();
  (document.getElementById("clock-time") as HTMLElement).textContent = `${pad(now.getUTCHours())}:${pad(now.getUTCMinutes())}`;
  (document.getElementById("clock-date") as HTMLElement).textContent = `${now.getUTCFullYear()}.${pad(now.getUTCMonth() + 1)}.${pad(now.getUTCDate())}`;
}

for (const button of document.querySelectorAll<HTMLButtonElement>(".window .min")) {
  button.addEventListener("click", () => button.closest(".window")?.classList.toggle("minimized"));
}

tick();
setInterval(tick, 1_000);
void refresh();
setInterval(() => void refresh(), REFRESH_MS);
