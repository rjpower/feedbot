const WORDS_PER_MINUTE = 220;

export function readingTime(words) {
  return Math.max(1, Math.round(words / WORDS_PER_MINUTE));
}

const DAY = 86400;

/** Recent things get a relative date; older things get their real one. */
export function shortDate(ts) {
  if (!ts) return "undated";
  const now = Date.now() / 1000;
  const age = now - ts;
  if (age < DAY) return "today";
  if (age < 2 * DAY) return "yesterday";
  if (age < 7 * DAY) return `${Math.floor(age / DAY)} days ago`;
  const d = new Date(ts * 1000);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  return d.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    year: sameYear ? undefined : "numeric",
  });
}

export function longDate(ts) {
  if (!ts) return "";
  return new Date(ts * 1000).toLocaleDateString(undefined, {
    day: "numeric",
    month: "long",
    year: "numeric",
  });
}

export function relativeTime(ts) {
  if (!ts) return "never";
  const age = Date.now() / 1000 - ts;
  if (age < 90) return "just now";
  if (age < 3600) return `${Math.floor(age / 60)}m ago`;
  if (age < DAY) return `${Math.floor(age / 3600)}h ago`;
  return `${Math.floor(age / DAY)}d ago`;
}

export function hostOf(url) {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return url;
  }
}

const INTERVALS = [
  [3600, "hourly"],
  [21600, "every 6h"],
  [43200, "every 12h"],
  [86400, "daily"],
  [259200, "every 3 days"],
  [604800, "weekly"],
];

export function intervalLabel(secs) {
  const hit = INTERVALS.find(([s]) => s === secs);
  if (hit) return hit[1];
  if (secs % 86400 === 0) return `every ${secs / 86400} days`;
  if (secs % 3600 === 0) return `every ${secs / 3600}h`;
  return `every ${Math.round(secs / 60)}m`;
}

export const INTERVAL_CHOICES = INTERVALS.map(([secs, label]) => ({ secs, label }));
