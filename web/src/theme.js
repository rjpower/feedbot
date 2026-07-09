import { reactive, watch } from "vue";

export const THEMES = [
  { id: "paper", label: "Paper" },
  { id: "sepia", label: "Sepia" },
  { id: "ink", label: "Ink" },
];

const MIN = 15;
const MAX = 26;

export const theme = reactive({
  id: localStorage.getItem("feedbot:theme") || "paper",
  size: Number(localStorage.getItem("feedbot:fontsize")) || 19,
});

function apply() {
  document.documentElement.dataset.theme = theme.id;
  document.documentElement.style.setProperty("--reader-size", `${theme.size}px`);
}

watch(
  () => [theme.id, theme.size],
  () => {
    localStorage.setItem("feedbot:theme", theme.id);
    localStorage.setItem("feedbot:fontsize", String(theme.size));
    apply();
  },
  { immediate: true },
);

export const cycleTheme = () => {
  const i = THEMES.findIndex((t) => t.id === theme.id);
  theme.id = THEMES[(i + 1) % THEMES.length].id;
};

export const grow = () => (theme.size = Math.min(MAX, theme.size + 1));
export const shrink = () => (theme.size = Math.max(MIN, theme.size - 1));
export const canGrow = () => theme.size < MAX;
export const canShrink = () => theme.size > MIN;
