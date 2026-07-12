<script setup>
import { ref } from "vue";
import { auth, setToken, bootstrapAuth } from "../api.js";

const value = ref("");
const error = ref("");
const busy = ref(false);

async function submit() {
  if (!value.value.trim()) return;
  busy.value = true;
  error.value = "";
  setToken(value.value);
  await bootstrapAuth();
  busy.value = false;
  if (auth.locked) {
    error.value = "That token was not accepted.";
    value.value = "";
  }
}
</script>

<template>
  <div class="lock">
    <div class="lock__inner rise">
      <span class="lock__mark" aria-hidden="true" />
      <h1 class="lock__title">feedbot</h1>
      <p class="lock__note meta">This reading room is private</p>
      <form class="lock__form" @submit.prevent="submit">
        <input
          v-model="value"
          class="field"
          type="password"
          placeholder="access token"
          autocomplete="current-password"
          aria-label="Access token"
          :disabled="busy"
        />
        <button class="btn btn--accent" type="submit" :disabled="busy || !value.trim()">
          {{ busy ? "Checking" : "Enter" }}
        </button>
      </form>
      <p v-if="error" class="lock__error meta">{{ error }}</p>
    </div>
  </div>
</template>

<style scoped>
.lock {
  /* See styles.css: vh is the fallback for engines without dvh. */
  min-height: 100vh;
  min-height: 100dvh;
  display: grid;
  place-items: center;
  padding: 1.5rem;
}
.lock__inner {
  width: min(24rem, 100%);
  text-align: center;
}
.lock__mark {
  display: block;
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: var(--accent);
  margin: 0 auto 1.6rem;
}
.lock__title {
  font-family: var(--display);
  font-weight: 600;
  font-variation-settings: "SOFT" 40, "WONK" 1;
  font-size: 2.4rem;
  letter-spacing: -0.02em;
  line-height: 1;
}
.lock__note {
  margin-top: 0.7rem;
}
.lock__form {
  margin-top: 2rem;
  display: flex;
  gap: 0.5rem;
}
.lock__form .field {
  text-align: center;
  letter-spacing: 0.2em;
}
.lock__error {
  margin-top: 1rem;
  color: var(--accent);
}
</style>
