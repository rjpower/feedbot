import { createApp } from "vue";
import { createRouter, createWebHistory } from "vue-router";
import App from "./App.vue";
import Inbox from "./views/Inbox.vue";
import Reader from "./views/Reader.vue";
import Sites from "./views/Sites.vue";
import { bootstrapAuth } from "./api.js";
import "./styles.css";

const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: "/", name: "inbox", component: Inbox },
    { path: "/read/:id", name: "read", component: Reader },
    { path: "/sites", name: "sites", component: Sites },
    { path: "/:pathMatch(.*)*", redirect: "/" },
  ],
  scrollBehavior: (to, from, saved) => saved ?? { top: 0 },
});

// Learn whether a token is needed before the first view paints, so we never
// flash the inbox and then yank it away.
bootstrapAuth().finally(() => {
  createApp(App).use(router).mount("#app");
});
