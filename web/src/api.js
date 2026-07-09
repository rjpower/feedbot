import { reactive } from "vue";

const TOKEN_KEY = "feedbot:token";

/** Shared auth state: the shell watches `locked` and swaps in the lock screen. */
export const auth = reactive({
  token: localStorage.getItem(TOKEN_KEY) || "",
  required: false,
  locked: false,
});

export function setToken(t) {
  auth.token = t.trim();
  localStorage.setItem(TOKEN_KEY, auth.token);
}

export function clearToken() {
  auth.token = "";
  localStorage.removeItem(TOKEN_KEY);
  auth.locked = auth.required;
}

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function request(path, { method = "GET", body, raw = false } = {}) {
  const headers = {};
  if (auth.token) headers["x-feedbot-token"] = auth.token;
  if (body !== undefined) headers["content-type"] = "application/json";

  const res = await fetch(`/api${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });

  if (res.status === 401) {
    auth.required = true;
    auth.locked = true;
    throw new ApiError(401, "unauthorized");
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      const j = await res.json();
      if (j.error) message = j.error;
    } catch {
      /* body wasn't json; the status line will do */
    }
    throw new ApiError(res.status, message);
  }
  if (raw) return res.blob();
  if (res.status === 204) return null;
  return res.json();
}

/** Downloads authenticate by query param, because <a download> sends no headers. */
export function downloadUrl(path) {
  const sep = path.includes("?") ? "&" : "?";
  return `/api${path}${auth.token ? `${sep}token=${encodeURIComponent(auth.token)}` : ""}`;
}

export const api = {
  authStatus: () => request("/auth"),
  stats: () => request("/stats"),

  sites: () => request("/sites"),
  addSite: (site) => request("/sites", { method: "POST", body: site }),
  patchSite: (id, patch) => request(`/sites/${id}`, { method: "PATCH", body: patch }),
  deleteSite: (id) => request(`/sites/${id}`, { method: "DELETE" }),
  crawlSite: (id) => request(`/sites/${id}/crawl`, { method: "POST" }),

  articles: (params = {}) => {
    const q = new URLSearchParams(
      Object.entries(params).filter(([, v]) => v !== undefined && v !== null && v !== ""),
    );
    return request(`/articles?${q}`);
  },
  article: (id, state) => request(`/articles/${id}?state=${encodeURIComponent(state || "unread")}`),
  setRead: (id, read) => request(`/articles/${id}/read`, { method: "POST", body: { read } }),
  setStarred: (id, starred) =>
    request(`/articles/${id}/star`, { method: "POST", body: { starred } }),
  deleteArticle: (id) => request(`/articles/${id}`, { method: "DELETE" }),
  markAllRead: (siteId) =>
    request("/articles/read-all", { method: "POST", body: { site_id: siteId ?? null } }),

  crawls: () => request("/crawls"),
};

/** Check the token we have, if any, before rendering anything else. */
export async function bootstrapAuth() {
  try {
    const s = await api.authStatus();
    auth.required = s.required;
    auth.locked = s.required && !s.ok;
  } catch {
    auth.locked = true;
  }
}
