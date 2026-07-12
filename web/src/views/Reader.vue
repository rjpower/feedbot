<script setup>
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { api, downloadUrl } from "../api.js";
import { longDate, readingTime, hostOf } from "../format.js";
import { theme, cycleTheme, grow, shrink, canGrow, canShrink } from "../theme.js";

const emit = defineEmits(["refresh-stats"]);
const route = useRoute();
const router = useRouter();

const article = ref(null);
const prevId = ref(null);
const nextId = ref(null);
const loading = ref(true);
const error = ref("");
const progress = ref(0);

const listState = computed(() => route.query.state || "unread");
const backLink = computed(() => ({ name: "inbox", query: { state: listState.value } }));

async function load(id) {
  loading.value = true;
  error.value = "";
  article.value = null;
  try {
    const data = await api.article(id, listState.value);
    prevId.value = data.prev_id;
    nextId.value = data.next_id;
    article.value = data;
    document.title = `${data.title} · feedbot`;
    window.scrollTo({ top: 0 });

    // Opening an article is what "reading" means. Do it once, quietly.
    if (!data.read_at) {
      await api.setRead(data.id, true);
      data.read_at = Math.floor(Date.now() / 1000);
      emit("refresh-stats");
    }
  } catch (e) {
    error.value = e.message;
  } finally {
    loading.value = false;
  }
}

function onScroll() {
  const h = document.documentElement.scrollHeight - window.innerHeight;
  progress.value = h > 0 ? Math.min(100, (window.scrollY / h) * 100) : 0;
}

function go(id) {
  if (id) router.push({ name: "read", params: { id }, query: { state: listState.value } });
}

function onKey(e) {
  if (e.target.matches("input, select, textarea")) return;
  if (e.key === "j" || e.key === "ArrowRight") go(nextId.value);
  else if (e.key === "k" || e.key === "ArrowLeft") go(prevId.value);
  else if (e.key === "Escape") router.push(backLink.value);
  else if (e.key === "s") toggleStar();
}

async function toggleStar() {
  const a = article.value;
  if (!a) return;
  a.starred = !a.starred;
  try {
    await api.setStarred(a.id, a.starred);
    emit("refresh-stats");
  } catch (e) {
    a.starred = !a.starred;
    error.value = e.message;
  }
}

async function markUnreadAndLeave() {
  const a = article.value;
  if (!a) return;
  await api.setRead(a.id, false);
  emit("refresh-stats");
  router.push(backLink.value);
}

onMounted(() => {
  load(route.params.id);
  window.addEventListener("scroll", onScroll, { passive: true });
  window.addEventListener("keydown", onKey);
});
onUnmounted(() => {
  window.removeEventListener("scroll", onScroll);
  window.removeEventListener("keydown", onKey);
  document.title = "feedbot";
});
watch(() => route.params.id, (id) => id && load(id));
</script>

<template>
  <div class="reader">
    <div class="progress" :style="{ transform: `scaleX(${progress / 100})` }" aria-hidden="true" />

    <nav class="toolbar">
      <RouterLink :to="backLink" class="btn btn--bare" title="Back to inbox (Esc)">← Inbox</RouterLink>

      <div class="toolbar__right" v-if="article">
        <button class="btn btn--bare" :disabled="!canShrink()" title="Smaller text" @click="shrink">A−</button>
        <span class="toolbar__size num">{{ theme.size }}</span>
        <button class="btn btn--bare" :disabled="!canGrow()" title="Larger text" @click="grow">A+</button>
        <button class="btn btn--bare" @click="cycleTheme" title="Change theme">◐</button>
        <button
          class="btn btn--bare"
          :class="{ 'iconbtn--on': article.starred }"
          :title="article.starred ? 'Unstar (s)' : 'Star (s)'"
          @click="toggleStar"
        >
          {{ article.starred ? "★" : "☆" }}
        </button>
        <a class="btn btn--bare" :href="downloadUrl(`/articles/${article.id}/mobi`)" title="Download for Kindle (.mobi, images embedded)">↓ MOBI</a>
        <a class="btn btn--bare" :href="downloadUrl(`/articles/${article.id}/epub`)" title="Download EPUB">↓ EPUB</a>
      </div>
    </nav>

    <p v-if="loading" class="status meta">Loading…</p>
    <p v-else-if="error" class="status status--bad meta">{{ error }}</p>

    <article v-else-if="article" class="article">
      <header class="head rise">
        <p class="head__kicker meta">
          <a :href="article.url" target="_blank" rel="noopener noreferrer">{{ hostOf(article.url) }}</a>
          <span aria-hidden="true">·</span>
          <span>{{ longDate(article.published_at) || "undated" }}</span>
          <span aria-hidden="true">·</span>
          <span>{{ readingTime(article.word_count) }} min read</span>
        </p>
        <h1 class="head__title">{{ article.title }}</h1>
        <p v-if="article.byline" class="head__byline">{{ article.byline }}</p>
        <hr class="rule head__rule" />
      </header>

      <!-- Sanitized server-side with ammonia before it ever reaches the db. -->
      <div class="prose rise" style="animation-delay: 80ms" v-html="article.content_html" />

      <footer class="foot">
        <hr class="rule" />
        <div class="foot__acts">
          <button class="btn" @click="markUnreadAndLeave">Keep unread</button>
          <a class="btn" :href="article.url" target="_blank" rel="noopener noreferrer">Original ↗</a>
        </div>
        <nav class="pager">
          <button class="pager__btn" :disabled="!prevId" @click="go(prevId)">
            <span class="meta">← Newer</span>
          </button>
          <button class="pager__btn pager__btn--r" :disabled="!nextId" @click="go(nextId)">
            <span class="meta">Older →</span>
          </button>
        </nav>
        <p class="foot__hint meta">j / k to move · s to star · esc to close</p>
      </footer>
    </article>
  </div>
</template>

<style scoped>
.progress {
  position: fixed;
  inset: 0 0 auto;
  height: 2px;
  background: var(--accent);
  transform-origin: 0 50%;
  z-index: 10;
  will-change: transform;
}

.toolbar {
  position: sticky;
  top: 0;
  z-index: 5;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem;
  padding: 0.9rem 0;
  margin-bottom: 1.5rem;
  /* Opaque first. Without color-mix (Chrome < 111) the toolbar would have no
     background at all and the article would scroll through it. */
  background: var(--bg);
  background: color-mix(in srgb, var(--bg) 92%, transparent);
  backdrop-filter: blur(8px);
  border-bottom: var(--rule-w) solid var(--rule);
}
.toolbar__right {
  display: flex;
  align-items: center;
  gap: 0.1rem;
}
.toolbar__size {
  font-size: 0.68rem;
  color: var(--ink-faint);
  width: 1.5rem;
  text-align: center;
}
.iconbtn--on {
  color: var(--accent);
}

.status {
  padding: 5rem 0;
  text-align: center;
}
.status--bad {
  color: var(--accent);
}

.article {
  max-width: var(--measure);
  margin: 0 auto;
}

.head__kicker {
  display: flex;
  flex-wrap: wrap;
  gap: 0.45rem;
}
.head__kicker a {
  color: var(--accent);
  text-decoration: none;
}
.head__kicker a:hover {
  text-decoration: underline;
}

.head__title {
  font-family: var(--display);
  font-variation-settings: "SOFT" 40, "WONK" 1;
  font-weight: 600;
  /* clamp() is Chrome 79; the Kindle may be a few versions short of it. */
  font-size: 2.3rem;
  font-size: clamp(1.9rem, 5.4vw, 2.7rem);
  line-height: 1.1;
  letter-spacing: -0.022em;
  margin-top: 0.9rem;
  text-wrap: balance;
}

.head__byline {
  margin-top: 0.9rem;
  font-style: italic;
  color: var(--ink-soft);
}

.head__rule {
  margin: 2rem 0 2.4rem;
}

/* --- the text itself ----------------------------------------------------- */

.prose {
  font-family: var(--body);
  font-size: var(--reader-size);
  line-height: 1.72;
  color: var(--ink);
  overflow-wrap: break-word;
}

.prose :deep(p) {
  margin-bottom: 1.35em;
}

/* An editorial flourish: the first paragraph opens with a drop cap. */
.prose :deep(> p:first-of-type::first-letter) {
  float: left;
  font-family: var(--display);
  font-variation-settings: "SOFT" 40, "WONK" 1;
  font-weight: 600;
  font-size: 3.4em;
  line-height: 0.82;
  padding: 0.06em 0.09em 0 0;
  color: var(--accent);
}

.prose :deep(h1),
.prose :deep(h2),
.prose :deep(h3),
.prose :deep(h4) {
  font-family: var(--display);
  font-variation-settings: "SOFT" 30, "WONK" 0;
  font-weight: 600;
  line-height: 1.2;
  letter-spacing: -0.01em;
  margin: 2.2em 0 0.7em;
}
.prose :deep(h1) { font-size: 1.55em; }
.prose :deep(h2) { font-size: 1.35em; }
.prose :deep(h3) { font-size: 1.15em; }

.prose :deep(a) {
  color: var(--ink);
  text-decoration: underline;
  text-decoration-color: var(--accent);
  text-underline-offset: 0.16em;
  text-decoration-thickness: 1px;
}
.prose :deep(a:hover) {
  color: var(--accent);
}

.prose :deep(img) {
  margin: 2em auto;
  border: var(--rule-w) solid var(--rule);
}

.prose :deep(figure) {
  margin: 2em 0;
}
.prose :deep(figcaption) {
  font-family: var(--meta);
  font-size: 0.78rem;
  color: var(--ink-faint);
  text-align: center;
  margin-top: 0.7em;
  line-height: 1.5;
}

.prose :deep(blockquote) {
  margin: 1.8em 0;
  padding-left: 1.4em;
  border-left: 2px solid var(--accent);
  color: var(--ink-soft);
  font-style: italic;
}

.prose :deep(pre) {
  background: var(--bg-sunk);
  border: var(--rule-w) solid var(--rule);
  padding: 1em;
  overflow-x: auto;
  font-size: 0.85em;
  line-height: 1.5;
  margin-bottom: 1.35em;
}
.prose :deep(code) {
  font-size: 0.88em;
  background: var(--bg-sunk);
  padding: 0.1em 0.3em;
}
.prose :deep(pre code) {
  background: none;
  padding: 0;
}

.prose :deep(ul),
.prose :deep(ol) {
  margin: 0 0 1.35em 1.4em;
}
.prose :deep(li) {
  margin-bottom: 0.5em;
}

.prose :deep(hr) {
  border: 0;
  border-top: var(--rule-w) solid var(--rule);
  margin: 2.5em 0;
}

.prose :deep(table) {
  max-width: 100%;
  border-collapse: collapse;
  margin: 2em auto;
  font-size: 0.9em;
  display: block;
  overflow-x: auto;
}
/* Chrome < 105 has no :has() and throws away every rule below that uses it.
   These two are the baseline it keeps: padded, left-aligned cells with a ruled
   header. A data table stays legible there; a layout table is merely roomier
   than it ought to be. */
.prose :deep(table th),
.prose :deep(table td) {
  padding: 0.5em 0.7em;
  text-align: left;
}
.prose :deep(table th) {
  border: var(--rule-w) solid var(--rule);
}

/* Blogger wraps captioned images in a borderless layout table. Only a table
   with a header row is really tabular data, so only that one gets rules. */
.prose :deep(table:has(th)) {
  width: 100%;
}
.prose :deep(table:has(th) th),
.prose :deep(table:has(th) td) {
  border: var(--rule-w) solid var(--rule);
  padding: 0.5em 0.7em;
  text-align: left;
}
/* ...and a layout table's caption cell reads as a caption. */
.prose :deep(table:not(:has(th)) td) {
  font-family: var(--meta);
  font-size: 0.78rem;
  color: var(--ink-faint);
  text-align: center;
  line-height: 1.5;
  padding-top: 0.6em;
}
.prose :deep(table:not(:has(th)) td:has(img)) {
  padding: 0;
}
.prose :deep(table img) {
  margin: 0 auto;
}

/* --- footer -------------------------------------------------------------- */

.foot {
  margin-top: 4rem;
}
.foot__acts {
  display: flex;
  gap: 0.5rem;
  justify-content: center;
  margin: 2rem 0;
}

.pager {
  display: flex;
  border-top: var(--rule-w) solid var(--rule);
}
.pager__btn {
  flex: 1;
  background: none;
  border: 0;
  cursor: pointer;
  padding: 1.6rem 0.5rem;
  text-align: left;
  color: var(--ink-faint);
  transition: background 0.2s var(--ease);
}
.pager__btn:hover:not(:disabled) {
  background: var(--bg-sunk);
}
.pager__btn:hover:not(:disabled) .meta {
  color: var(--accent);
}
.pager__btn:disabled {
  opacity: 0.3;
  cursor: not-allowed;
}
.pager__btn--r {
  text-align: right;
  border-left: var(--rule-w) solid var(--rule);
}

.foot__hint {
  text-align: center;
  margin-top: 2rem;
  font-size: 0.63rem;
}
@media (hover: none) {
  .foot__hint {
    display: none;
  }
}
</style>
