// The board's dev server: the page, Restate's ingress (port 8080) at
// /ingress and its admin API (port 9070) at /admin, on the same origin, so
// the browser needs no CORS.
import { defineConfig } from 'vite';

export default defineConfig({
  server: {
    proxy: {
      '/ingress': { target: 'http://localhost:8080', rewrite: (path) => path.replace(/^\/ingress/, '') },
      '/admin': { target: 'http://localhost:9070', rewrite: (path) => path.replace(/^\/admin/, '') },
    },
  },
});
