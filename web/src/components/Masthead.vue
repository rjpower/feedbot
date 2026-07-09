<script setup>
import { RouterLink, useRoute } from "vue-router";
import { theme, THEMES, cycleTheme } from "../theme.js";

defineProps({ stats: { type: Object, default: null } });
const route = useRoute();
const themeLabel = () => THEMES.find((t) => t.id === theme.id)?.label ?? "Paper";
</script>

<template>
  <header class="masthead">
    <div class="masthead__top">
      <RouterLink to="/" class="wordmark" aria-label="feedbot home">
        <span class="wordmark__text">feedbot</span>
        <span class="wordmark__dot" aria-hidden="true" />
      </RouterLink>

      <nav class="nav">
        <RouterLink to="/" class="nav__link" :class="{ 'nav__link--on': route.name === 'inbox' }">
          Inbox
          <span v-if="stats?.unread" class="nav__count num">{{ stats.unread }}</span>
        </RouterLink>
        <RouterLink
          to="/sites"
          class="nav__link"
          :class="{ 'nav__link--on': route.name === 'sites' }"
        >
          Sites
          <span v-if="stats?.sites" class="nav__count num">{{ stats.sites }}</span>
        </RouterLink>
        <button class="nav__link nav__link--btn" @click="cycleTheme" :title="`Theme: ${themeLabel()}`">
          {{ themeLabel() }}
        </button>
      </nav>
    </div>
    <hr class="rule" />
    <p v-if="stats" class="masthead__strap meta">
      {{ stats.articles }} articles · {{ stats.unread }} unread · {{ stats.starred }} starred
    </p>
  </header>
</template>

<style scoped>
.masthead {
  padding-top: 2.4rem;
}

.masthead__top {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 1rem;
  flex-wrap: wrap;
  margin-bottom: 0.7rem;
}

.wordmark {
  font-family: var(--display);
  font-weight: 600;
  font-size: clamp(1.9rem, 5vw, 2.5rem);
  font-variation-settings: "SOFT" 40, "WONK" 1;
  letter-spacing: -0.02em;
  text-decoration: none;
  line-height: 1;
  display: inline-flex;
  align-items: baseline;
  gap: 0.14em;
}
.wordmark__dot {
  width: 0.2em;
  height: 0.2em;
  border-radius: 50%;
  background: var(--accent);
  translate: 0 -0.05em;
}

.nav {
  display: flex;
  align-items: center;
  gap: 1.4rem;
}

.nav__link {
  font-family: var(--meta);
  font-size: 0.72rem;
  letter-spacing: 0.13em;
  text-transform: uppercase;
  color: var(--ink-faint);
  text-decoration: none;
  padding-bottom: 0.2rem;
  border: 0;
  border-bottom: 2px solid transparent;
  background: none;
  cursor: pointer;
  transition: color 0.18s var(--ease);
  display: inline-flex;
  align-items: center;
  gap: 0.45rem;
}
.nav__link:hover {
  color: var(--ink);
}
.nav__link--on {
  color: var(--ink);
  border-bottom-color: var(--accent);
}
.nav__link--btn {
  min-width: 3.4rem;
  justify-content: flex-end;
}

.nav__count {
  font-size: 0.68rem;
  color: var(--accent);
  letter-spacing: 0;
}

.masthead__strap {
  margin-top: 0.6rem;
}

@media (max-width: 480px) {
  .nav {
    gap: 1rem;
  }
}
</style>
