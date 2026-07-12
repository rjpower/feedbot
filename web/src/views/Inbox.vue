<script setup>
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { RouterLink, useRoute, useRouter } from "vue-router";
import { api, downloadUrl } from "../api.js";
import { readingTime, shortDate } from "../format.js";

const emit = defineEmits(["refresh-stats"]);
const route = useRoute();
const router = useRouter();

const PAGE = 50;
const STATES = [
  { id: "unread", label: "Unread" },
  { id: "all", label: "All" },
  { id: "starred", label: "Starred" },
];

const articles = ref([]);
const sites = ref([]);
const loading = ref(true);
const error = ref("");
const exhausted = ref(false);
const search = ref(route.query.q || "");

const state = computed(() => route.query.state || "unread");
const siteId = computed(() => (route.query.site ? Number(route.query.site) : undefined));

function navigate(patch) {
  router.replace({ query: { ...route.query, ...patch } });
}

async function load({ append = false } = {}) {
  loading.value = true;
  error.value = "";
  try {
    const rows = await api.articles({
      state: state.value,
      site_id: siteId.value,
      q: route.query.q,
      limit: PAGE,
      offset: append ? articles.value.length : 0,
    });
    articles.value = append ? [...articles.value, ...rows] : rows;
    exhausted.value = rows.length < PAGE;
  } catch (e) {
    error.value = e.message;
  } finally {
    loading.value = false;
  }
}

watch(() => [route.query.state, route.query.site, route.query.q], () => load(), { deep: true });

// Debounce so a fast typist doesn't issue a query per keystroke.
let timer;
watch(search, (v) => {
  clearTimeout(timer);
  timer = setTimeout(() => navigate({ q: v || undefined }), 250);
});

onMounted(async () => {
  // Re-attach to an export that was in flight when we last had this view open.
  const pending = localStorage.getItem(EXPORT_KEY);
  if (pending) {
    exportJob.value = { id: pending, status: "running", phase: "selecting", done: 0, total: 0 };
    watchExport();
  }
  await load();
  try {
    sites.value = await api.sites();
  } catch {
    /* the site filter is a nicety; the list is the point */
  }
});

async function toggleStar(a) {
  a.starred = !a.starred;
  try {
    await api.setStarred(a.id, a.starred);
    emit("refresh-stats");
  } catch (e) {
    a.starred = !a.starred; // put it back
    error.value = e.message;
  }
}

async function toggleRead(a) {
  const wasRead = !!a.read_at;
  a.read_at = wasRead ? null : Math.floor(Date.now() / 1000);
  try {
    await api.setRead(a.id, !wasRead);
    emit("refresh-stats");
    if (state.value === "unread" && !wasRead) {
      articles.value = articles.value.filter((x) => x.id !== a.id);
    }
  } catch (e) {
    a.read_at = wasRead ? Math.floor(Date.now() / 1000) : null;
    error.value = e.message;
  }
}

async function markAllRead() {
  const n = articles.value.length;
  if (!n || !confirm(`Mark ${state.value === "unread" ? n : "all"} articles as read?`)) return;
  await api.markAllRead(siteId.value);
  emit("refresh-stats");
  await load();
}

// A whole-list export builds server-side — it fetches and transcodes every
// post's images, which takes minutes — so we start a job, watch its progress,
// and hand the finished .mobi to the browser. The job id lives in localStorage
// so the progress survives a reload or a round-trip into the reader. The export
// takes up to per_site of each site's newest (not a flat cut) so every site
// lands in the book, not just the most prolific few.
const EXPORT_KEY = "feedbot:export";
const exportJob = ref(null);
let exportTimer;

const exporting = computed(() => exportJob.value?.status === "running");

const exportLabel = computed(() => {
  const j = exportJob.value;
  if (!j) return "";
  if (j.status === "failed") return j.error || "Export failed";
  if (j.status === "done") return `Ready · ${(j.size / 1048576).toFixed(1)} MB`;
  if (j.phase === "assembling") return "Assembling…";
  if (j.total) return `Fetching images · ${j.done}/${j.total}`;
  return "Preparing…";
});

const exportPct = computed(() => {
  const j = exportJob.value;
  if (!j || !j.total) return 0;
  if (j.phase === "assembling" || j.status === "done") return 100;
  return Math.round((j.done / j.total) * 100);
});

function exportUrl(id) {
  return downloadUrl(`/export/mobi/${id}/download`);
}

// <a download> click rather than location change, so the download starts without
// navigating away from the inbox. The visible link is the fallback if it's blocked.
function triggerDownload(id) {
  const a = document.createElement("a");
  a.href = exportUrl(id);
  a.download = "";
  document.body.appendChild(a);
  a.click();
  a.remove();
}

function watchExport() {
  clearTimeout(exportTimer);
  exportTimer = setTimeout(async () => {
    const id = exportJob.value?.id;
    if (!id) return;
    try {
      const j = await api.exportStatus(id);
      exportJob.value = j;
      if (j.status === "running") {
        watchExport();
      } else {
        localStorage.removeItem(EXPORT_KEY);
        if (j.status === "done") triggerDownload(id);
      }
    } catch {
      // Aged out of the server's cache, or the server restarted — forget it.
      exportJob.value = null;
      localStorage.removeItem(EXPORT_KEY);
    }
  }, 1000);
}

async function startExport() {
  error.value = "";
  try {
    const params = { state: state.value, per_site: 10 };
    if (siteId.value) params.site_id = siteId.value;
    const j = await api.startExport(params);
    exportJob.value = j;
    localStorage.setItem(EXPORT_KEY, j.id);
    watchExport();
  } catch (e) {
    error.value = e.message;
  }
}

onUnmounted(() => clearTimeout(exportTimer));

const emptyMessage = computed(() => {
  if (route.query.q) return `Nothing matches “${route.query.q}”.`;
  if (state.value === "starred") return "Nothing starred yet.";
  if (state.value === "unread") return "Inbox zero. Nothing left to read.";
  return "No articles yet — add a site and let it crawl.";
});
</script>

<template>
  <section>
    <div class="bar">
      <div class="tabs">
        <button
          v-for="s in STATES"
          :key="s.id"
          class="tab"
          :class="{ 'tab--on': state === s.id }"
          @click="navigate({ state: s.id })"
        >
          {{ s.label }}
        </button>
      </div>

      <div class="tools">
        <input v-model="search" class="field field--search" type="search" placeholder="Search…" aria-label="Search articles" />
        <select
          v-if="sites.length > 1"
          class="field field--site"
          :value="route.query.site || ''"
          aria-label="Filter by site"
          @change="navigate({ site: $event.target.value || undefined })"
        >
          <option value="">All sites</option>
          <option v-for="s in sites" :key="s.id" :value="s.id">{{ s.name }}</option>
        </select>
      </div>
    </div>

    <div v-if="articles.length" class="actions">
      <div v-if="exportJob" class="export" :class="`export--${exportJob.status}`">
        <div v-if="exporting" class="export__track" role="progressbar" :aria-valuenow="exportPct">
          <span class="export__fill" :style="{ width: `${exportPct}%` }" />
        </div>
        <span class="export__label meta">{{ exportLabel }}</span>
        <a
          v-if="exportJob.status === 'done'"
          class="btn btn--bare"
          :href="exportUrl(exportJob.id)"
          title="Download the finished .mobi"
        >↓ .mobi</a>
        <button
          v-if="!exporting"
          class="btn btn--bare"
          title="Build it again"
          @click="startExport"
        >↻</button>
      </div>
      <button
        v-else
        class="btn btn--bare"
        title="Build a Kindle .mobi: up to 10 newest per site, images embedded"
        @click="startExport"
      >↓ MOBI</button>
      <button class="btn btn--bare" @click="markAllRead">Mark all read</button>
    </div>

    <p v-if="error" class="notice notice--bad meta">{{ error }}</p>

    <ol v-if="articles.length" class="index">
      <li
        v-for="(a, i) in articles"
        :key="a.id"
        class="row rise"
        :class="{ 'row--read': a.read_at }"
        :style="{ animationDelay: `${Math.min(i, 12) * 22}ms` }"
      >
        <span class="row__num num" aria-hidden="true">{{ String(i + 1).padStart(2, "0") }}</span>

        <div class="row__body">
          <RouterLink :to="{ name: 'read', params: { id: a.id }, query: { state } }" class="row__link">
            <span v-if="!a.read_at" class="row__dot" aria-label="unread" />
            <h2 class="row__title">{{ a.title }}</h2>
          </RouterLink>
          <p class="row__meta meta">
            <span>{{ a.site_name }}</span>
            <span aria-hidden="true">·</span>
            <span>{{ shortDate(a.published_at) }}</span>
            <span aria-hidden="true">·</span>
            <span>{{ readingTime(a.word_count) }} min</span>
            <span v-if="a.byline" aria-hidden="true">·</span>
            <span v-if="a.byline">{{ a.byline }}</span>
          </p>
          <p v-if="a.excerpt" class="row__excerpt">{{ a.excerpt }}</p>
        </div>

        <div class="row__acts">
          <button
            class="iconbtn"
            :class="{ 'iconbtn--on': a.starred }"
            :title="a.starred ? 'Unstar' : 'Star'"
            :aria-pressed="a.starred"
            @click="toggleStar(a)"
          >
            {{ a.starred ? "★" : "☆" }}
          </button>
          <button
            class="iconbtn"
            :title="a.read_at ? 'Mark unread' : 'Mark read'"
            :aria-pressed="!!a.read_at"
            @click="toggleRead(a)"
          >
            {{ a.read_at ? "◌" : "●" }}
          </button>
        </div>
      </li>
    </ol>

    <p v-else-if="!loading" class="empty">{{ emptyMessage }}</p>

    <p v-if="loading" class="empty meta">Loading…</p>

    <div v-if="articles.length && !exhausted" class="more">
      <button class="btn" :disabled="loading" @click="load({ append: true })">Load more</button>
    </div>
  </section>
</template>

<style scoped>
.bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  flex-wrap: wrap;
  margin: 2rem 0 0.5rem;
}

.tabs {
  display: flex;
  gap: 0.2rem;
}
.tab {
  font-family: var(--meta);
  font-size: 0.72rem;
  letter-spacing: 0.13em;
  text-transform: uppercase;
  background: none;
  border: 0;
  border-bottom: 2px solid transparent;
  color: var(--ink-faint);
  padding: 0.3rem 0.7rem 0.35rem;
  cursor: pointer;
  transition: color 0.18s var(--ease);
}
.tab:hover {
  color: var(--ink);
}
.tab--on {
  color: var(--ink);
  border-bottom-color: var(--accent);
}

.tools {
  display: flex;
  gap: 0.5rem;
}
.field--search {
  width: 11rem;
}
.field--site {
  width: 9rem;
}

.actions {
  display: flex;
  gap: 0.4rem;
  align-items: center;
  justify-content: flex-end;
  margin-bottom: 0.4rem;
}

.export {
  display: flex;
  align-items: center;
  gap: 0.5rem;
}
.export__track {
  width: 7rem;
  height: 0.3rem;
  background: var(--bg-sunk);
  border: var(--rule-w) solid var(--rule);
  overflow: hidden;
}
.export__fill {
  display: block;
  height: 100%;
  background: var(--accent);
  transition: width 0.4s var(--ease);
}
.export__label {
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.export--failed .export__label {
  color: var(--accent);
}

.notice {
  padding: 0.6rem 0;
}
.notice--bad {
  color: var(--accent);
}

.index {
  list-style: none;
  padding: 0;
  border-top: var(--rule-w) solid var(--rule);
}

.row {
  display: grid;
  grid-template-columns: 3rem 1fr auto;
  gap: 0 0.6rem;
  align-items: start;
  padding: 1.35rem 0.5rem 1.4rem 0;
  border-bottom: var(--rule-w) solid var(--rule);
  transition: background 0.2s var(--ease);
}
.row:hover {
  background: var(--bg-sunk);
}
.row--read .row__title {
  color: var(--ink-soft);
  font-weight: 400;
}
.row--read .row__excerpt {
  color: var(--ink-faint);
}

.row__num {
  font-size: 0.72rem;
  color: var(--ink-faint);
  padding-top: 0.5rem;
  padding-left: 0.3rem;
  transition: color 0.2s var(--ease);
}
.row:hover .row__num {
  color: var(--accent);
}

.row__link {
  text-decoration: none;
  display: flex;
  gap: 0.55rem;
  align-items: baseline;
}

.row__dot {
  flex: none;
  width: 0.42rem;
  height: 0.42rem;
  border-radius: 50%;
  background: var(--accent);
  translate: 0 -0.18em;
}

.row__title {
  font-family: var(--display);
  font-variation-settings: "SOFT" 30, "WONK" 0;
  font-weight: 500;
  font-size: 1.32rem;
  line-height: 1.25;
  letter-spacing: -0.01em;
  text-wrap: balance;
  transition: color 0.18s var(--ease);
}
.row__link:hover .row__title {
  color: var(--accent);
}

.row__meta {
  display: flex;
  flex-wrap: wrap;
  gap: 0.4rem;
  margin-top: 0.5rem;
}

.row__excerpt {
  margin-top: 0.55rem;
  color: var(--ink-soft);
  font-size: 0.95rem;
  line-height: 1.55;
  max-width: 44rem;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

.row__acts {
  display: flex;
  gap: 0.1rem;
  padding-top: 0.25rem;
  opacity: 0;
  transition: opacity 0.18s var(--ease);
}
.row:hover .row__acts,
.row:focus-within .row__acts {
  opacity: 1;
}

.iconbtn {
  background: none;
  border: 0;
  cursor: pointer;
  color: var(--ink-faint);
  font-size: 0.95rem;
  line-height: 1;
  padding: 0.4rem;
  transition: color 0.16s var(--ease);
}
.iconbtn:hover {
  color: var(--accent);
}
.iconbtn--on {
  color: var(--accent);
}

.empty {
  padding: 5rem 0;
  text-align: center;
  color: var(--ink-faint);
  font-size: 1.05rem;
}

.more {
  display: flex;
  justify-content: center;
  margin-top: 2rem;
}

/* Touch devices have no hover, so the row actions must always be visible. */
@media (hover: none) {
  .row__acts {
    opacity: 1;
  }
}

@media (max-width: 640px) {
  .row {
    grid-template-columns: 1.7rem 1fr auto;
  }
  .row__num {
    font-size: 0.62rem;
    padding-left: 0;
  }
  .row__title {
    font-size: 1.15rem;
  }
  .row__excerpt {
    display: none;
  }
  .tools {
    width: 100%;
  }
  .field--search,
  .field--site {
    width: 100%;
  }
}
</style>
