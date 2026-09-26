// @ts-check
import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

// Static marketing shell only. No adapter, no islands on marketing pages:
// every page under src/pages/ ships zero client-side JavaScript.
export default defineConfig({
  output: 'static',
  // PLACEHOLDER ORIGIN — must be set to the real domain before launch.
  // Canonical links are built from this, and a wrong origin is worse than none:
  // it tells crawlers the real page lives somewhere it does not. (It is also
  // what a future sitemap integration would build its URLs from.)
  site: 'https://apikita.id',
  vite: {
    plugins: [tailwindcss()],
  },
});
