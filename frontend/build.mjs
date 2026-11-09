import { build } from 'esbuild';
import { copyFileSync, mkdirSync } from 'node:fs';
mkdirSync('dist', { recursive: true });
await build({ entryPoints: ['src/main.ts'], bundle: true, minify: true, format: 'iife', target: 'es2022', outfile: 'dist/app.js', legalComments: 'none' });
copyFileSync('src/index.html', 'dist/index.html');
copyFileSync('src/style.css', 'dist/style.css');
console.log('frontend built -> frontend/dist');
