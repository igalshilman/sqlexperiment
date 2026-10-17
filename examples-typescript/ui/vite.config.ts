// The chat page's dev server: the page, and Restate's ingress (port 8080)
// at /ingress on the same origin, so the browser needs no CORS.
import { defineConfig } from 'vite';

export default defineConfig({
  server: {
    proxy: {
      '/ingress': { target: 'http://localhost:8080', rewrite: (path) => path.replace(/^\/ingress/, '') },
    },
  },
});
