import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import wasm from 'vite-plugin-wasm';
import topLevelAwait from 'vite-plugin-top-level-await';
import dtsPlugin from 'vite-plugin-dts';

// https://vitejs.dev/config/
export default defineConfig(({ mode }) => {
    return {
        build: {
            lib: {
                entry: './src/main.ts',
                name: 'IronRemoteDesktop',
                formats: ['es'],
            },
            sourcemap: mode === 'development',
        },
        server: {
            fs: {
                strict: false,
            },
        },
        plugins: [
            svelte(),
            wasm(),
            topLevelAwait(),
            dtsPlugin({
                rollupTypes: true,
            }),
        ],
    };
});
