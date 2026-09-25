// @ts-check
import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

// Static marketing shell only. No adapter, no islands on marketing pages:
// every page under src/pages/ ships zero client-side JavaScript.
export default defineConfig({
  output: 'static',
  vite: {
    plugins: [tailwindcss()],
  },
});
