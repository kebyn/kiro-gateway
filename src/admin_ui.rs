use axum::{http::header, response::IntoResponse};

const INDEX: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Kiro Gateway Admin</title>
<style>body{font:16px system-ui;margin:2rem;max-width:60rem}main{display:grid;gap:1rem}input,button{padding:.55rem}pre{background:#f5f5f5;padding:1rem;overflow:auto}</style></head>
<body><main><h1>Kiro Gateway Admin</h1><form id="login"><input id="key" type="password" autocomplete="current-password" placeholder="Admin API key"><button>Sign in</button></form><p id="status">Signed out</p><button id="refresh" hidden>Refresh credential</button><pre id="credential"></pre></main>
<script>let csrf='';const status=document.querySelector('#status'),out=document.querySelector('#credential'),refresh=document.querySelector('#refresh');async function session(){const r=await fetch('/admin/auth/session',{credentials:'same-origin'});if(!r.ok){status.textContent='Signed out';refresh.hidden=true;return}const v=await r.json();csrf=v.csrf_token;status.textContent='Signed in';refresh.hidden=false;const c=await fetch('/api/admin/credential',{credentials:'same-origin'});out.textContent=JSON.stringify(await c.json(),null,2)}document.querySelector('#login').onsubmit=async e=>{e.preventDefault();const r=await fetch('/admin/auth/login',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({api_key:document.querySelector('#key').value}),credentials:'same-origin'});if(r.ok) await session();else status.textContent='Login failed'};refresh.onclick=async()=>{await fetch('/api/admin/credential/refresh',{method:'POST',headers:{'x-csrf-token':csrf},credentials:'same-origin'});await session()};session();</script></body></html>"#;

pub async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX)
}
