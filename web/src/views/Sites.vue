<script setup>
import { onMounted, onUnmounted, ref } from "vue";
import { RouterLink } from "vue-router";
import { api } from "../api.js";
import { INTERVAL_CHOICES, intervalLabel, relativeTime, hostOf } from "../format.js";

const emit = defineEmits(["refresh-stats"]);

const sites = ref([]);
const crawls = ref([]);
const loading = ref(true);
const error = ref("");
const busy = ref(new Set());
const expanded = ref(null);

const draft = ref({ url: "", name: "", interval_secs: 86400 });
const adding = ref(false);

async function load() {
  try {
    [sites.value, crawls.value] = await Promise.all([api.sites(), api.crawls()]);
    error.value = "";
  } catch (e) {
    error.value = e.message;
  } finally {
    loading.value = false;
  }
}

// A crawl runs in the background, so poll while one is in flight.
let poll;
onMounted(async () => {
  await load();
  poll = setInterval(load, 5000);
});
onUnmounted(() => clearInterval(poll));

async function addSite() {
  if (!draft.value.url.trim()) return;
  adding.value = true;
  error.value = "";
  try {
    await api.addSite({
      url: draft.value.url.trim(),
      name: draft.value.name.trim() || undefined,
      interval_secs: draft.value.interval_secs,
    });
    draft.value = { url: "", name: "", interval_secs: 86400 };
    await load();
    emit("refresh-stats");
  } catch (e) {
    error.value = e.message;
  } finally {
    adding.value = false;
  }
}

async function withBusy(id, fn) {
  busy.value = new Set(busy.value).add(id);
  try {
    await fn();
    error.value = "";
  } catch (e) {
    error.value = e.message;
  } finally {
    const next = new Set(busy.value);
    next.delete(id);
    busy.value = next;
    await load();
  }
}

// A 409 means the scheduler beat us to it — say so rather than looking broken.
const crawlNow = (s) => withBusy(s.id, () => api.crawlSite(s.id));
const toggleEnabled = (s) => withBusy(s.id, () => api.patchSite(s.id, { enabled: !s.enabled }));

async function savePolicy(s) {
  await withBusy(s.id, () =>
    api.patchSite(s.id, {
      name: s.name,
      interval_secs: Number(s.interval_secs),
      max_new_per_crawl: Number(s.max_new_per_crawl),
      url_pattern: s.url_pattern?.trim() || null,
      feed_url: s.feed_url?.trim() || null,
    }),
  );
  expanded.value = null;
}

async function removeSite(s) {
  if (!confirm(`Delete “${s.name}” and all ${s.article_count} of its articles?`)) return;
  await withBusy(s.id, () => api.deleteSite(s.id));
  emit("refresh-stats");
}

const lastCrawl = (id) => crawls.value.find((c) => c.site_id === id);
</script>

<template>
  <section>
    <h1 class="title">Sites</h1>
    <p class="lede">
      feedbot visits each site on its own schedule, reads the feed and the front page, and keeps
      whatever looks like a top-level post. Fetches never leave the site.
    </p>

    <form class="add" @submit.prevent="addSite">
      <input v-model="draft.url" class="field add__url" type="url" placeholder="https://example.com/" aria-label="Site URL" required />
      <input v-model="draft.name" class="field add__name" type="text" placeholder="Name (optional)" aria-label="Site name" />
      <select v-model.number="draft.interval_secs" class="field add__int" aria-label="Crawl interval">
        <option v-for="c in INTERVAL_CHOICES" :key="c.secs" :value="c.secs">{{ c.label }}</option>
      </select>
      <button class="btn btn--accent" type="submit" :disabled="adding || !draft.url.trim()">
        {{ adding ? "Adding…" : "Add site" }}
      </button>
    </form>

    <p v-if="error" class="notice meta">{{ error }}</p>

    <p v-if="loading" class="empty meta">Loading…</p>
    <p v-else-if="!sites.length" class="empty">No sites yet. Add one above.</p>

    <ul v-else class="sites">
      <li v-for="s in sites" :key="s.id" class="site" :class="{ 'site--off': !s.enabled }">
        <div class="site__main">
          <div class="site__id">
            <h2 class="site__name">{{ s.name }}</h2>
            <a class="site__host meta" :href="s.url" target="_blank" rel="noopener noreferrer">{{ hostOf(s.url) }} ↗</a>
          </div>

          <p class="site__stats meta">
            <RouterLink :to="{ name: 'inbox', query: { state: 'all', site: s.id } }" class="site__stat">
              <span class="num">{{ s.article_count }}</span> articles
            </RouterLink>
            <span v-if="s.unread_count" class="site__stat site__stat--accent">
              <span class="num">{{ s.unread_count }}</span> unread
            </span>
            <span>{{ intervalLabel(s.interval_secs) }}</span>
            <span>crawled {{ relativeTime(s.last_crawled_at) }}</span>
            <span v-if="lastCrawl(s.id) && lastCrawl(s.id).ok === false" class="site__stat--accent">
              failed: {{ lastCrawl(s.id).error }}
            </span>
          </p>
        </div>

        <div class="site__acts">
          <button class="btn btn--bare" :disabled="busy.has(s.id)" @click="crawlNow(s)">
            <span :class="{ spin: busy.has(s.id) }" style="display: inline-block">↻</span>
            Crawl
          </button>
          <button class="btn btn--bare" @click="expanded = expanded === s.id ? null : s.id">
            {{ expanded === s.id ? "Close" : "Policy" }}
          </button>
          <button class="btn btn--bare" :title="s.enabled ? 'Pause' : 'Resume'" @click="toggleEnabled(s)">
            {{ s.enabled ? "Pause" : "Resume" }}
          </button>
          <button class="btn btn--bare btn--danger" @click="removeSite(s)">Delete</button>
        </div>

        <div v-if="expanded === s.id" class="policy">
          <label class="policy__row">
            <span class="meta">Name</span>
            <input v-model="s.name" class="field" type="text" />
          </label>
          <label class="policy__row">
            <span class="meta">Every</span>
            <select v-model.number="s.interval_secs" class="field">
              <option v-for="c in INTERVAL_CHOICES" :key="c.secs" :value="c.secs">{{ c.label }}</option>
            </select>
          </label>
          <label class="policy__row">
            <span class="meta">Max new per crawl</span>
            <input v-model.number="s.max_new_per_crawl" class="field" type="number" min="1" max="200" />
          </label>
          <label class="policy__row">
            <span class="meta">URL pattern</span>
            <input v-model="s.url_pattern" class="field" type="text" placeholder="regex — blank uses the built-in heuristic" />
          </label>
          <label class="policy__row">
            <span class="meta">Feed URL</span>
            <input v-model="s.feed_url" class="field" type="text" placeholder="autodiscovered" />
          </label>
          <p class="policy__hint meta">
            A pattern must match the whole article URL. Off-site links and assets are always refused.
          </p>
          <div class="policy__acts">
            <button class="btn btn--accent" :disabled="busy.has(s.id)" @click="savePolicy(s)">Save</button>
            <button class="btn" @click="expanded = null">Cancel</button>
          </div>
        </div>
      </li>
    </ul>

    <section v-if="crawls.length" class="history">
      <h2 class="history__title meta">Recent crawls</h2>
      <ul class="history__list">
        <li v-for="c in crawls.slice(0, 8)" :key="c.id" class="history__row">
          <span class="history__dot" :class="c.ok === false ? 'bad' : c.ok ? 'good' : 'pending'" />
          <span class="history__site">{{ c.site_name }}</span>
          <span class="meta">{{ relativeTime(c.started_at) }}</span>
          <span class="meta">
            <template v-if="c.ok === false">{{ c.error }}</template>
            <template v-else-if="c.ok"><span class="num">+{{ c.added }}</span> of <span class="num">{{ c.discovered }}</span> found</template>
            <template v-else>running…</template>
          </span>
        </li>
      </ul>
    </section>
  </section>
</template>

<style scoped>
.title {
  font-family: var(--display);
  font-variation-settings: "SOFT" 40, "WONK" 1;
  font-weight: 600;
  font-size: 2rem;
  letter-spacing: -0.02em;
  margin: 2.4rem 0 0.7rem;
}
.lede {
  color: var(--ink-soft);
  max-width: 40rem;
  margin-bottom: 2.2rem;
}

.add {
  display: grid;
  grid-template-columns: 2fr 1.2fr 1fr auto;
  gap: 0.5rem;
  padding-bottom: 2rem;
  border-bottom: var(--rule-w) solid var(--rule);
}

.notice {
  color: var(--accent);
  padding: 0.8rem 0;
}

.sites {
  list-style: none;
  padding: 0;
}

.site {
  display: grid;
  grid-template-columns: 1fr auto;
  gap: 0.6rem 1rem;
  padding: 1.5rem 0;
  border-bottom: var(--rule-w) solid var(--rule);
}
.site--off {
  opacity: 0.5;
}

.site__id {
  display: flex;
  align-items: baseline;
  gap: 0.7rem;
  flex-wrap: wrap;
}
.site__name {
  font-family: var(--display);
  font-variation-settings: "SOFT" 30;
  font-weight: 600;
  font-size: 1.28rem;
  letter-spacing: -0.01em;
}
.site__host {
  text-decoration: none;
}
.site__host:hover {
  color: var(--accent);
}

.site__stats {
  display: flex;
  flex-wrap: wrap;
  gap: 0.9rem;
  margin-top: 0.6rem;
}
.site__stat {
  color: inherit;
  text-decoration: none;
}
.site__stat:hover {
  color: var(--ink);
}
.site__stat--accent {
  color: var(--accent);
}

.site__acts {
  display: flex;
  align-items: flex-start;
  gap: 0.1rem;
  flex-wrap: wrap;
}
.btn--danger:hover:not(:disabled) {
  color: var(--accent);
}

.policy {
  grid-column: 1 / -1;
  background: var(--bg-sunk);
  border: var(--rule-w) solid var(--rule);
  padding: 1.2rem;
  margin-top: 0.8rem;
  display: grid;
  gap: 0.8rem;
}
.policy__row {
  display: grid;
  grid-template-columns: 10rem 1fr;
  align-items: center;
  gap: 0.8rem;
}
.policy__hint {
  color: var(--ink-faint);
  line-height: 1.6;
  text-transform: none;
  letter-spacing: 0.02em;
  font-size: 0.78rem;
}
.policy__acts {
  display: flex;
  gap: 0.5rem;
}

.empty {
  padding: 4rem 0;
  text-align: center;
  color: var(--ink-faint);
}

.history {
  margin-top: 3.5rem;
}
.history__title {
  padding-bottom: 0.8rem;
  border-bottom: var(--rule-w) solid var(--rule);
}
.history__list {
  list-style: none;
  padding: 0;
}
.history__row {
  display: grid;
  grid-template-columns: 0.5rem 12rem 6rem 1fr;
  align-items: center;
  gap: 0.9rem;
  padding: 0.6rem 0;
  border-bottom: var(--rule-w) solid var(--rule);
  font-size: 0.9rem;
}
.history__dot {
  width: 0.42rem;
  height: 0.42rem;
  border-radius: 50%;
}
.history__dot.good {
  background: var(--ink-faint);
}
.history__dot.bad {
  background: var(--accent);
}
.history__dot.pending {
  background: var(--accent);
  animation: pulse 1.2s ease-in-out infinite;
}
@keyframes pulse {
  50% {
    opacity: 0.25;
  }
}
.history__site {
  font-weight: 500;
}

@media (max-width: 720px) {
  .add {
    grid-template-columns: 1fr;
  }
  .site {
    grid-template-columns: 1fr;
  }
  .policy__row {
    grid-template-columns: 1fr;
    gap: 0.3rem;
  }
  .history__row {
    grid-template-columns: 0.5rem 1fr;
    row-gap: 0.2rem;
  }
}
</style>
