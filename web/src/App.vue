<script setup>
import { onMounted, ref } from "vue";
import { RouterView, useRoute } from "vue-router";
import Masthead from "./components/Masthead.vue";
import Lock from "./components/Lock.vue";
import { api, auth } from "./api.js";

const route = useRoute();
const stats = ref(null);

async function refreshStats() {
  try {
    stats.value = await api.stats();
  } catch {
    /* the masthead counts are decoration; never block the app on them */
  }
}

onMounted(refreshStats);
</script>

<template>
  <Lock v-if="auth.locked" />
  <div v-else class="shell">
    <!-- The reader is a page of its own; the masthead would only interrupt it. -->
    <Masthead v-if="route.name !== 'read'" :stats="stats" />
    <RouterView @refresh-stats="refreshStats" />
  </div>
</template>
