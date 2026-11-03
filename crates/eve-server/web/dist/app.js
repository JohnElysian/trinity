// The dashboard: polls the server's JSON and fills the windows in, in the
// manner of the EVE client: overview-style lists sorted by any column, the
// game's security colours, [CORP] and <ALLIANCE> tickers, EVE time.
const REFRESH_MS = 15000;
const numbers = new Intl.NumberFormat("en-US");
async function get(path) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 10000);
    try {
        const response = await fetch(path, { cache: "no-store", signal: controller.signal });
        if (!response.ok)
            throw new Error(`${path}: ${response.status}`);
        return (await response.json());
    }
    finally {
        clearTimeout(timeout);
    }
}
function span(text, className) {
    const e = document.createElement("span");
    e.textContent = text;
    e.className = className;
    return e;
}
const corpTicker = (t) => span(t ? ` [${t}]` : "", "tick");
const allianceTicker = (t) => span(t ? ` <${t}>` : "", "tick");
/// The security status colours of the client's map and overview, 1.0 to 0.0.
const SECURITY_COLOURS = ["#f00000", "#d73000", "#f04800", "#f06000", "#d77700", "#efef00", "#8fef2f", "#00f000", "#00ef47", "#48f0c0", "#2fefef"];
function security(s) {
    const rounded = Math.max(-1, Math.min(1, Math.round(s * 10) / 10));
    const e = span(rounded.toFixed(1), "sec");
    e.style.color = SECURITY_COLOURS[Math.max(0, Math.round(rounded * 10))];
    return e;
}
const pad = (n) => String(n).padStart(2, "0");
/// EVE time is UTC, written YYYY.MM.DD HH:MM.
function eveTime(ms) {
    if (!ms) {
        return "";
    }
    const d = new Date(ms);
    return `${d.getUTCFullYear()}.${pad(d.getUTCMonth() + 1)}.${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
}
const lists = new Map();
/// Shows a list, sorted as its reader last chose (or by its first column).
function list(id, columns, items, empty, sortBy = 0, descending = false) {
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
function draw(id) {
    const l = lists.get(id);
    const table = document.getElementById(id);
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
const count = (n) => [numbers.format(n)];
const compact = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });
const isk = (n) => `${compact.format(n)} ISK`;
const price = (n) => (n >= 1000 ? compact.format(n) : n.toFixed(2));
// ---- windows ----------------------------------------------------------------------
function showStatus(s) {
    const rows = [
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
    const dl = document.getElementById("status");
    dl.replaceChildren(...rows.flatMap(([label, value]) => {
        const dt = document.createElement("dt");
        dt.textContent = label;
        const dd = document.createElement("dd");
        dd.textContent = value;
        return [dt, dd];
    }));
    const online = s.players + s.bots.online;
    const line = document.getElementById("statusline");
    line.textContent = `Status: OK, ${numbers.format(online)} players${s.bots.running ? "" : " (simulation paused)"}`;
}
function uptime(seconds) {
    const d = Math.floor(seconds / 86400);
    const h = Math.floor((seconds % 86400) / 3600);
    const m = Math.floor((seconds % 3600) / 60);
    return d > 0 ? `${d}d ${h}h ${m}m` : h > 0 ? `${h}h ${m}m` : `${m}m`;
}
/// Refresh healthy panels even if another endpoint fails. Keep their last good
/// contents on failure, and report which requests failed in the status line.
async function updatePanel(path, render) {
    try {
        await render(await get(path));
        return null;
    }
    catch (e) {
        return e instanceof Error ? e.message : String(e);
    }
}
let refreshing = false;
async function refresh() {
    if (refreshing)
        return;
    refreshing = true;
    try {
        const failures = (await Promise.all([
            updatePanel("/api/summary", async (summary) => {
                showStatus(summary);
            }),
            updatePanel("/api/wars", async (wars) => {
                list("wars", [
                    { title: "Aggressor", key: (w) => w.declaredBy, cell: (w) => [w.declaredBy, allianceTicker(w.declaredByTicker)] },
                    { title: "", key: () => "", cell: () => [span("vs", "hostile")] },
                    { title: "Defender", key: (w) => w.against, cell: (w) => [w.against, allianceTicker(w.againstTicker)] },
                    { title: "Declared", key: (w) => w.declared ?? 0, cell: (w) => [eveTime(w.declared)] },
                    { title: "Ends", key: (w) => w.ends ?? 0, cell: (w) => [eveTime(w.ends)] },
                    { title: "Losses", num: true, key: (w) => w.losses, cell: (w) => count(w.losses) },
                    { title: "Type", key: (w) => (w.mutual ? 1 : 0), cell: (w) => [span(w.bots ? "mutual (simulated)" : w.mutual ? "mutual" : "declared", "dim")] },
                ], wars, "There are no wars.", 3, true);
            }),
            updatePanel("/api/players", async (players) => {
                list("players", [
                    { title: "Name", key: (p) => p.name, cell: (p) => [p.name] },
                    { title: "Corporation", key: (p) => p.ticker ?? "", cell: (p) => [corpTicker(p.ticker), p.alliance ? span(` ${p.alliance}`, "dim") : ""] },
                    { title: "Location", key: (p) => p.system ?? "", cell: (p) => [security(p.systemSecurity ?? 0), ` ${p.system ?? ""}`, p.docked ? span(" (docked)", "dim") : ""] },
                    { title: "Ship", key: (p) => p.ship ?? "", cell: (p) => [p.ship ?? ""] },
                ], players, "No pilots are logged in.");
            }),
            updatePanel("/api/systems", async (systems) => {
                list("systems", [
                    { title: "System", key: (s) => s.name, cell: (s) => [security(s.security), ` ${s.name}`] },
                    { title: "Region", key: (s) => s.region, cell: (s) => [span(s.region, "dim")] },
                    { title: "In space", num: true, key: (s) => s.inSpace, cell: (s) => count(s.inSpace) },
                    { title: "Docked", num: true, key: (s) => s.docked, cell: (s) => count(s.docked) },
                    { title: "Total", num: true, key: (s) => s.inSpace + s.docked, cell: (s) => count(s.inSpace + s.docked) },
                ], systems, "No one is about.", 4, true);
            }),
            updatePanel("/api/alliances", async (alliances) => {
                list("alliances", [
                    { title: "Alliance", key: (a) => a.name, cell: (a) => [a.name, allianceTicker(a.ticker)] },
                    { title: "Holds", key: (a) => a.region ?? "", cell: (a) => [a.region ?? span(a.kind === "players" ? "" : a.kind, "dim")] },
                    { title: "Corps", num: true, key: (a) => a.corporations, cell: (a) => count(a.corporations) },
                    { title: "Pilots", num: true, key: (a) => a.pilots, cell: (a) => count(a.pilots) },
                    { title: "Systems", num: true, key: (a) => a.systems, cell: (a) => (a.systems > 0 ? count(a.systems) : [span("-", "dim")]) },
                ], alliances, "No alliances have been founded.", 3, true);
            }),
            updatePanel("/api/corporations", async (corporations) => {
                list("corporations", [
                    { title: "Corporation", key: (c) => c.name, cell: (c) => [c.name, corpTicker(c.ticker), c.bots ? "" : span(" (players)", "dim")] },
                    { title: "Alliance", key: (c) => c.alliance ?? "", cell: (c) => [allianceTicker(c.alliance)] },
                    { title: "Members", num: true, key: (c) => c.members, cell: (c) => count(c.members) },
                ], corporations, "No corporations have been founded.", 2, true);
            }),
            updatePanel("/api/sieges", async (sieges) => {
                list("sieges", [
                    { title: "System", key: (s) => s.system, cell: (s) => [security(s.security), ` ${s.system}`, span(` ${s.region}`, "dim")] },
                    { title: "Attacker", key: (s) => s.attacker, cell: (s) => [s.attacker] },
                    { title: "Defender", key: (s) => s.defender, cell: (s) => [s.defender] },
                    {
                        title: "When",
                        key: (s) => s.starts,
                        cell: (s) => s.fighting
                            ? [span(`BATTLE ${s.attackers ?? 0} vs ${s.defenders ?? 0}`, "hostile")]
                            : [eveTime(s.starts)],
                    },
                ], sieges, "No sieges are planned.", 3, false);
                await refreshMap(sieges);
            }),
            updatePanel("/api/news", async (news) => {
                list("news", [
                    { title: "Time", key: (n) => n.time, cell: (n) => [eveTime(n.time)] },
                    { title: "Report", key: (n) => n.text, cell: (n) => [n.text] },
                ], news, "Nothing has happened in null-sec yet.", 0, true);
            }),
            updatePanel("/api/kills", async (kills) => {
                list("kills", [
                    { title: "Time", key: (k) => k.time ?? 0, cell: (k) => [eveTime(k.time)] },
                    { title: "Victim", key: (k) => k.victim ?? "", cell: (k) => [k.victim ?? "", k.victimCorporation ? span(` (${k.victimCorporation})`, "dim") : ""] },
                    { title: "Destroyed", key: (k) => k.ship ?? "", cell: (k) => [k.ship ?? "", k.pod ? span(" (pod)", "hostile") : ""] },
                    { title: "System", key: (k) => k.system ?? "", cell: (k) => [k.system ?? ""] },
                    { title: "Final blow", key: (k) => k.finalBlow ?? "", cell: (k) => [k.finalBlow ?? ""] },
                ], kills, "No kills have been reported.", 0, true);
            }),
            ...refreshBots(),
        ])).filter((failure) => failure !== null);
        if (failures.length) {
            setText("statusline", `Status: some panels unavailable, retrying (${failures.join("; ")})`);
        }
    }
    finally {
        refreshing = false;
    }
}
const SVG_NS = "http://www.w3.org/2000/svg";
/// A system's box in a region, and the room kept around it.
const BOX_W = 66;
const BOX_H = 26;
const BOX_GAP = 10;
let atlas = null;
let holders = new Map();
let allianceNames = new Map();
let besieged = new Set();
/// The region shown; null for all of New Eden.
let mapRegion = null;
/// The alliance picked out in the legend.
let pickedAlliance = null;
const marks = new Map();
let homeView = { x: 0, y: 0, w: 1, h: 1 };
let view = homeView;
const mapSvg = () => document.getElementById("map");
function svgEl(tag, attrs = {}) {
    const e = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs)) {
        e.setAttribute(k, String(v));
    }
    return e;
}
function buildAtlas(data) {
    const stars = new Map();
    const byName = new Map();
    for (const [id, name, region, constellation, x, z, sec] of data.systems) {
        // Seen from above, north up: x to the right, z up the page.
        stars.set(id, { id, name, region, constellation, x, y: -z, sec, links: [] });
        byName.set(name, id);
    }
    for (const [a, b] of data.jumps) {
        stars.get(a)?.links.push(b);
        stars.get(b)?.links.push(a);
    }
    const regionStars = new Map();
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
function allianceColour(id, alpha = 1) {
    const hue = (Math.imul(id, 2654435761) >>> 0) % 360;
    const light = 48 + ((Math.imul(id, 40503) >>> 0) % 3) * 9;
    return `hsla(${hue}, 72%, ${light}%, ${alpha})`;
}
/// An unclaimed system's colour: empire space greyed blue, low-sec brown,
/// null-sec dark.
function unclaimedColour(s) {
    if (s.sec >= 0.45) {
        return mapRegion === null ? "#4a5566" : "rgba(90, 110, 140, .18)";
    }
    if (s.sec > 0) {
        return mapRegion === null ? "#5e5040" : "rgba(140, 110, 70, .16)";
    }
    return mapRegion === null ? "#33363d" : "rgba(255, 255, 255, .04)";
}
function securityColour(s) {
    const rounded = Math.max(0, Math.min(1, Math.round(s * 10) / 10));
    return SECURITY_COLOURS[Math.round(rounded * 10)];
}
async function refreshMap(sieges) {
    try {
        if (!atlas) {
            atlas = buildAtlas(await get("/api/map"));
            const select = document.getElementById("map-region");
            const names = [...atlas.regionStars.keys()]
                .map((id) => [id, atlas?.regions.get(id) ?? String(id)])
                .sort((a, b) => a[1].localeCompare(b[1]));
            select.append(...names.map(([id, name]) => new Option(name, String(id))));
            drawMap();
        }
        const sov = await get("/api/sov");
        holders = new Map(sov.systems);
        allianceNames = new Map(sov.alliances.map(([id, name, ticker]) => [id, { name, ticker }]));
        if (pickedAlliance !== null && !allianceNames.has(pickedAlliance)) {
            pickedAlliance = null;
        }
        const known = atlas;
        besieged = new Set(sieges.map((s) => known.byName.get(s.system)).filter((id) => id !== undefined));
        paintMap();
    }
    catch (e) {
        setText("map-summary", `The map did not load (${e instanceof Error ? e.message : String(e)})`);
    }
}
function drawMap() {
    if (!atlas) {
        return;
    }
    marks.clear();
    mapSvg().replaceChildren();
    if (mapRegion === null) {
        drawGalaxy(atlas);
    }
    else {
        drawRegion(atlas, mapRegion);
    }
    setView(homeView);
    paintMap();
}
function bounds(points, pad) {
    const xs = points.map((p) => p.x);
    const ys = points.map((p) => p.y);
    if (!points.length)
        return { x: -pad, y: -pad, w: pad * 2 || 1, h: pad * 2 || 1 };
    const x = Math.min(...xs) - pad;
    const y = Math.min(...ys) - pad;
    return { x, y, w: Math.max(...xs) + pad - x, h: Math.max(...ys) + pad - y };
}
function drawGalaxy(a) {
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
        }
        else {
            between += d;
        }
    }
    svg.append(svgEl("path", { class: "jumps", d: within, "stroke-width": r * 0.3 }), svgEl("path", { class: "jumps region", d: between, "stroke-width": r * 0.3, "stroke-dasharray": `${r * 2} ${r * 1.5}` }));
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
/// Spreads the boxes so none overlap, each pulled gently back to where the
/// stars put it.
function relax(nodes) {
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
                }
                else {
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
function drawRegion(a, region) {
    const svg = mapSvg();
    const inside = a.regionStars.get(region) ?? [];
    const insideIds = new Set(inside.map((s) => s.id));
    const outside = new Map();
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
    const nodes = [...inside, ...outside.values()].map((s) => {
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
            }
            else if (n.star.constellation !== m.star.constellation) {
                constellation += d;
            }
            else {
                plain += d;
            }
        }
    }
    svg.append(svgEl("path", { class: "jumps", d: plain, "stroke-width": 1.2 }), svgEl("path", { class: "jumps constellation", d: constellation, "stroke-width": 1.2 }), svgEl("path", { class: "jumps region", d: regional, "stroke-width": 1.2 }));
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
        }
        else {
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
function paintMap() {
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
    const held = new Map();
    for (const s of shown) {
        const h = holders.get(s.id);
        if (h !== undefined) {
            held.set(h, (held.get(h) ?? 0) + 1);
        }
    }
    const claimed = [...held.values()].reduce((t, n) => t + n, 0);
    setText("map-summary", galaxy
        ? `${numbers.format(claimed)} systems held by ${numbers.format(held.size)} alliances`
        : `${atlas.regions.get(mapRegion ?? 0) ?? ""}: ${numbers.format(claimed)} of ${numbers.format(shown.length)} systems claimed`);
    const legend = document.getElementById("map-legend");
    const rows = [...held.entries()].sort((x, y) => y[1] - x[1] || x[0] - y[0]);
    legend.replaceChildren(...rows.map(([id, n]) => {
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
    }));
    if (rows.length === 0) {
        const li = document.createElement("li");
        li.append(span(galaxy ? "Nobody holds any space yet." : "Nobody holds space here.", "dim"));
        legend.append(li);
    }
}
function showRegion(region) {
    mapRegion = region;
    document.getElementById("map-region").value = region === null ? "" : String(region);
    drawMap();
}
function describe(id) {
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
function setView(v) {
    view = v;
    mapSvg().setAttribute("viewBox", `${v.x} ${v.y} ${v.w} ${v.h}`);
}
/// A point on the screen in the map's own units.
function mapPoint(clientX, clientY) {
    const svg = mapSvg();
    const m = svg.getScreenCTM();
    const p = new DOMPoint(clientX, clientY);
    return m ? p.matrixTransform(m.inverse()) : p;
}
{
    const svg = mapSvg();
    let drag = null;
    let dragged = false;
    svg.addEventListener("wheel", (e) => {
        e.preventDefault();
        const p = mapPoint(e.clientX, e.clientY);
        const k = e.deltaY < 0 ? 0.8 : 1.25;
        const w = Math.min(homeView.w * 2, Math.max(homeView.w / 60, view.w * k));
        const f = w / view.w;
        setView({ x: p.x - (p.x - view.x) * f, y: p.y - (p.y - view.y) * f, w, h: view.h * f });
    }, { passive: false });
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
        const target = e.target.closest("[data-id]");
        const region = e.target.closest("[data-region]:not([data-id])");
        setText("map-hover", target
            ? describe(Number(target.getAttribute("data-id")))
            : region
                ? `${atlas?.regions.get(Number(region.getAttribute("data-region"))) ?? ""}: click to open`
                : "Scroll to zoom, drag to pan. Click a system to open its region; click an alliance to pick out its space.");
    });
    const endDrag = (e) => {
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
        const el = e.target;
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
    document.getElementById("map-region").addEventListener("change", (e) => {
        const v = e.target.value;
        showRegion(v === "" ? null : Number(v));
    });
    document.getElementById("map-reset").addEventListener("click", () => setView(homeView));
}
// ---- the bots' windows -------------------------------------------------------------
let market = null;
let chosenPilot = null;
let controlsEnabled = false;
let followedNow = [];
function refreshBots() {
    return [
        updatePanel("/api/killboard", (board) => {
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
        updatePanel("/api/market", (mkt) => {
            if (mkt && mkt.hubs) {
                market = mkt;
                const select = document.getElementById("hub");
                const before = select.value;
                select.replaceChildren(...mkt.hubs.map((h) => new Option(h.station, h.station)));
                select.value = mkt.hubs.some((h) => h.station === before) ? before : (mkt.hubs[0]?.station ?? "");
                showMarket();
                setText("market-summary", `${compact.format(mkt.minedM3)} m3 mined, ${compact.format(mkt.hauledM3)} m3 hauled, ${numbers.format(mkt.hullsBuilt)} hulls built, ` +
                    `${numbers.format(mkt.hullsBought)} bought (${numbers.format(mkt.hullsFromNpcs)} from NPCs); players bought ${isk(mkt.soldToPlayers)} and sold ${isk(mkt.boughtFromPlayers)}; ` +
                    `${numbers.format(mkt.orders)} orders up; ${isk(mkt.corpTax)} in corporation tax`);
                setText("market-shortages", mkt.shortages.length ? `Short of: ${mkt.shortages.map((s) => `${s.name} (${s.wanted})`).join(", ")}` : "");
            }
        }),
        updatePanel("/api/relations", (rel) => {
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
        updatePanel("/api/followed", (followed) => {
            if (followed) {
                followedNow = followed;
                if (chosenPilot === null || !followed.some((f) => f.id === chosenPilot)) {
                    chosenPilot = followed[0]?.id ?? null;
                }
                showFollowed();
            }
        }),
        updatePanel("/api/events", (events) => {
            if (events) {
                controlsEnabled = events.controls;
                const state = events.controls ? "Controls are on." : "Controls are off: set bots.dashboardControls in trinity.yml to use them.";
                setText("controls-state", state);
                for (const b of document.querySelectorAll("#w-control .body button, [data-control]")) {
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
function setText(id, text) {
    const e = document.getElementById(id);
    if (e) {
        e.textContent = text;
    }
}
function showMarket() {
    if (!market) {
        return;
    }
    const chosen = document.getElementById("hub").value;
    const hub = market.hubs.find((h) => h.station === chosen) ?? market.hubs[0];
    list("market", [
        { title: "Item", key: (g) => g.name, cell: (g) => [g.name] },
        { title: "Kind", key: (g) => g.kind, cell: (g) => [span(g.kind, "dim")] },
        { title: "In stock", num: true, key: (g) => g.stock, cell: (g) => count(g.stock) },
        { title: "Bots sell at", num: true, key: (g) => g.ask, cell: (g) => [price(g.ask)] },
        { title: "Bots buy at", num: true, key: (g) => (g.kind === "ship" ? 0 : g.bid), cell: (g) => [g.kind === "ship" ? "" : price(g.bid)] },
        { title: "NPC ask", num: true, key: (g) => g.npcAsk, cell: (g) => [span(price(g.npcAsk), "dim")] },
    ], hub ? hub.goods : [], "The bots hold nothing here yet.", 2, true);
}
function showFollowed() {
    list("followed", [
        { title: "Pilot", key: (f) => f.name, cell: (f) => [pilotButton(f), span(` ${f.role}`, "dim")] },
        { title: "Corporation", key: (f) => f.corporation, cell: (f) => [f.corporation, f.alliance ? span(` ${f.alliance}`, "dim") : ""] },
        { title: "Ship", key: (f) => f.ship, cell: (f) => [f.ship, span(` T${f.tier}`, "dim")] },
        { title: "Where", key: (f) => f.system, cell: (f) => [security(f.security), ` ${f.system}`, f.station ? span(" (docked)", "dim") : ""] },
        { title: "Doing", key: (f) => f.activity, cell: (f) => [f.online ? f.activity : span("offline", "dim")] },
        { title: "Wallet", num: true, key: (f) => f.isk, cell: (f) => [isk(f.isk)] },
        { title: "", key: () => "", cell: (f) => [unfollowButton(f)] },
    ], followedNow, "Nobody followed. Search for a pilot's name above.");
    const chosen = followedNow.find((f) => f.id === chosenPilot);
    const ol = document.getElementById("history");
    ol.replaceChildren(...(chosen?.history ?? []).map((h) => {
        const li = document.createElement("li");
        const t = document.createElement("time");
        t.textContent = eveTime(h.time);
        li.append(t, h.text);
        return li;
    }));
}
function pilotButton(f) {
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
function unfollowButton(f) {
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
async function control(what) {
    const reply = document.getElementById("control-reply");
    if (!controlsEnabled) {
        reply.textContent = "Dashboard controls are off.";
        reply.className = "note hostile";
        return;
    }
    try {
        const response = await fetch(`/api/control?do=${encodeURIComponent(what)}`, { method: "POST" });
        const body = (await response.json());
        reply.textContent = body.ok ?? body.error ?? "";
        reply.className = body.ok ? "note" : "note hostile";
    }
    catch (e) {
        reply.textContent = `The server did not answer (${e instanceof Error ? e.message : String(e)})`;
        reply.className = "note hostile";
    }
    setTimeout(() => void refresh(), 1500);
}
let searchTimer;
let searchVersion = 0;
async function searchPilots() {
    const version = ++searchVersion;
    const q = document.getElementById("pilot-search").value.trim();
    const out = document.getElementById("pilot-results");
    if (q.length < 2) {
        out.replaceChildren();
        return;
    }
    try {
        const found = await get(`/api/pilots?q=${encodeURIComponent(q)}`);
        if (version !== searchVersion)
            return;
        out.replaceChildren(...found.map((p) => {
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
        }));
        if (!found.length)
            out.append(span("No pilots found.", "dim"));
    }
    catch (e) {
        if (version !== searchVersion)
            return;
        out.replaceChildren(span(`Search unavailable (${e instanceof Error ? e.message : String(e)})`, "hostile"));
    }
}
document.getElementById("hub").addEventListener("change", showMarket);
document.getElementById("pilot-search").addEventListener("input", () => {
    clearTimeout(searchTimer);
    // Invalidate the prior request as soon as the query changes, during debounce too.
    ++searchVersion;
    document.getElementById("pilot-results").replaceChildren();
    searchTimer = window.setTimeout(() => void searchPilots(), 250);
});
for (const b of document.querySelectorAll("#event-buttons button")) {
    b.addEventListener("click", () => void control(b.dataset.do ?? ""));
}
document.getElementById("war-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const f = new FormData(e.target);
    void control(`war ${f.get("a")} | ${f.get("b")}`);
});
document.getElementById("density-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const f = new FormData(e.target);
    void control(`density ${f.get("region")} ${f.get("factor")}`);
});
// ---- the neocom clock and the minimize buttons ------------------------------------
function tick() {
    const now = new Date();
    document.getElementById("clock-time").textContent = `${pad(now.getUTCHours())}:${pad(now.getUTCMinutes())}`;
    document.getElementById("clock-date").textContent = `${now.getUTCFullYear()}.${pad(now.getUTCMonth() + 1)}.${pad(now.getUTCDate())}`;
}
for (const button of document.querySelectorAll(".window .min")) {
    button.addEventListener("click", () => button.closest(".window")?.classList.toggle("minimized"));
}
tick();
setInterval(tick, 1000);
void refresh();
setInterval(() => void refresh(), REFRESH_MS);
export {};
