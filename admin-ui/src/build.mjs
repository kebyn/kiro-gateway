import { mkdir, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const root = resolve(new URL('.', import.meta.url).pathname, '..');
const output = resolve(root, 'dist');
await mkdir(output, { recursive: true });
const html = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Kiro Gateway Admin</title>
<style>body{font:16px system-ui;margin:2rem;max-width:60rem}main{display:grid;gap:1rem}button{padding:.5rem 1rem}pre{background:#f5f5f5;padding:1rem;overflow:auto}</style></head>
<body><main><h1>Kiro Gateway Admin</h1><p id="status">Checking session…</p><button id="refresh">Refresh credential</button><pre id="credential"></pre></main>
<script type="module">const status=document.querySelector('#status'),out=document.querySelector('#credential');async function load(){const r=await fetch('/admin/auth/session',{credentials:'same-origin'});if(!r.ok){status.textContent='Not signed in';return}status.textContent='Signed in';const c=await fetch('/api/admin/credential',{credentials:'same-origin'});out.textContent=JSON.stringify(await c.json(),null,2)}document.querySelector('#refresh').onclick=async()=>{await fetch('/api/admin/credential/refresh',{method:'POST',headers:{'x-csrf-token':sessionStorage.getItem('csrf')||''},credentials:'same-origin'});await load()};load();</script></body></html>`;
await writeFile(resolve(output, 'index.html'), html);

