import * as esbuild from 'esbuild';
import { copyFileSync, mkdirSync } from 'fs';

const isDev = process.argv.includes('--dev');

mkdirSync('dist', { recursive: true });

await esbuild.build({
  entryPoints: ['src/index.tsx'],
  bundle: true,
  outdir: 'dist',
  minify: !isDev,
  sourcemap: isDev,
  loader: { '.tsx': 'tsx', '.ts': 'ts', '.css': 'css' },
  jsx: 'automatic',
  jsxImportSource: 'preact',
  define: {
    'process.env.NODE_ENV': isDev ? '"development"' : '"production"',
  },
});

copyFileSync('index.html', 'dist/index.html');
copyFileSync('src/style.css', 'dist/style.css');
