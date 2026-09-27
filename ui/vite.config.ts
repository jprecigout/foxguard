import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// En production, le manager sert le bundle ET l'API sur la même origine : il
// n'y a donc ni CORS à configurer ni URL d'API à injecter, les appels partent
// en chemin relatif (`/api/...`).
//
// En développement, `vite dev` sert l'interface sur son propre port : le proxy
// ci-dessous rétablit cette même origine unique, pour que le code d'appel soit
// rigoureusement identique dans les deux cas.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/api": {
        target: process.env.FOXGUARD_API ?? "http://localhost:8090",
        changeOrigin: true,
      },
    },
  },
  build: {
    // Le manager sert ce dossier (voir `[server] ui_dir` de
    // manager-config.toml).
    outDir: "dist",
  },
});
