import { defineConfig } from 'vitest/config';
import vue from '@vitejs/plugin-vue';

const apiTarget = process.env.MOZART_WEB_API_TARGET;
const apiOrigin = apiTarget === undefined ? undefined : new URL(apiTarget).origin;

export default defineConfig(() => ({
  plugins: [vue()],
  server: {
    // 开发代理由启动环境指定；不提供内置模拟后端。
    proxy: apiTarget !== undefined
      ? { '/api/v1': {
        target: apiTarget,
        changeOrigin: true,
        configure(proxy) {
          proxy.on('proxyReq', request => {
            // 本机开发代理保持上游 Host/Origin 一致；生产资源与 API 直接同源。
            request.setHeader('Origin', apiOrigin!);
          });
        },
      } }
      : undefined,
  },
  test: { environment: 'jsdom', include: ['tests/**/*.test.ts'], restoreMocks: true },
}));
