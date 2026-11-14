import { defineConfig } from "vite";
import { resolve } from "path";

export default defineConfig({
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    // 必须忽略 Rust 构建产物：cargo 写 target/**/deps/*.dll 时 Vite 会去 watch
    // 这个被锁定的文件，抛 EBUSY 直接把 dev server 打挂（tauri dev 随即启动失败）。
    watch: { ignored: ["**/src-tauri/**"] },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
        captcha: resolve(__dirname, "captcha.html"),
      },
    },
  },
});
