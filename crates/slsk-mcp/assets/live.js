// Moves measured progress between the server's patches. Each patch carries
// a measurement and when it was taken; this draws from it at the display's
// rate, and the next patch re-anchors it. Nothing here is invented: a bar
// stops at its total, at a rate of 0, and two report intervals past its
// last measurement, and a count only ever shows a measured or deadline time.
//
//   data-bytes, data-total, data-rate (bytes a second), data-at (ms since the
//   epoch, 0 while it does not move): a transfer. On a <progress> it sets
//   the value; elsewhere data-show says what to write: "bytes" or "eta".
//   data-since (ms since the epoch): time on the current stage.
//   data-until (ms since the epoch): time left until a deadline.
//
// The server's times are its own clock's. A transfer is drawn from when
// this page received its measurement, so a client clock that differs does
// not matter there; stage times and deadlines are read against the server's
// clock, estimated from the page's data-now and each fresh measurement.

// The engine measures a transfer's rate once a second.
const REPORT = 1000;
const HOLD = 2 * REPORT;

const human = (b) => {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let u = 0;
  while (b >= 1024 && u < units.length - 1) {
    b /= 1024;
    u++;
  }
  return u === 0 ? `${Math.round(b)} B` : `${b.toFixed(1)} ${units[u]}`;
};

const span = (ms) => {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
};

const write = (el, text) => {
  if (el.textContent !== text) el.textContent = text;
};

// The server's clock less this one's, in ms.
let skew = document.body?.dataset.now ? +document.body.dataset.now - Date.now() : 0;
const serverNow = () => Date.now() + skew;

// When each element's current measurement arrived, on this page's clock.
const seen = new WeakMap();
const arrived = (el) => {
  const at = +el.dataset.at;
  const last = seen.get(el);
  if (last && last.at === at) return last.t;
  const t = performance.now();
  seen.set(el, { at, t });
  // A measurement is drawn as it is sent: its stamp is the server's now.
  if (at > 0) skew = at - Date.now();
  return t;
};

let drawing = false;
function frame() {
  if (document.hidden) {
    drawing = false;
    return;
  }
  const now = performance.now();
  let moving = false;
  for (const el of document.querySelectorAll("[data-rate]")) {
    const d = el.dataset;
    const [bytes, total, rate] = [+d.bytes, +d.total, +d.rate];
    const since = now - arrived(el);
    const ahead = Math.min(since, HOLD);
    const at_now = rate > 0 ? Math.min(total, bytes + (rate * ahead) / 1000) : bytes;
    if (rate > 0 && since < HOLD && at_now < total) moving = true;
    if (el.tagName === "PROGRESS") {
      el.value = at_now;
    } else if (d.show === "eta") {
      write(el, rate > 0 && at_now < total ? `${span(((total - at_now) / rate) * 1000)} left` : "");
    } else {
      write(el, human(at_now));
    }
  }
  if (moving) {
    requestAnimationFrame(frame);
  } else {
    drawing = false;
  }
}

let ticking = null;
function tick() {
  ticking = null;
  if (document.hidden) return;
  const now = serverNow();
  let any = false;
  for (const el of document.querySelectorAll("[data-since]")) {
    write(el, span(now - +el.dataset.since));
    any = true;
  }
  for (const el of document.querySelectorAll("[data-until]")) {
    const left = +el.dataset.until - now;
    write(el, span(left));
    if (left > 0) any = true;
  }
  // On the next whole second, so a count steps evenly.
  if (any) ticking = setTimeout(tick, 1000 - (now % 1000));
}

function start() {
  if (document.hidden) return;
  if (!drawing && document.querySelector("[data-rate]")) {
    drawing = true;
    requestAnimationFrame(frame);
  }
  if (ticking === null) tick();
}

new MutationObserver(start).observe(document.documentElement, {
  subtree: true,
  childList: true,
  attributes: true,
  attributeFilter: ["data-at", "data-since", "data-until"],
});
document.addEventListener("visibilitychange", start);
start();
