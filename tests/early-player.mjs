// Deterministic browser test: headers/audio arrive while the HTTP response stays open.
import {chromium} from 'playwright';
import assert from 'node:assert/strict';
let cancelled=false,ended=false,requested=false;
const server=Bun.serve({port:0,idleTimeout:60,fetch:async req=>{
 const path=new URL(req.url).pathname;
 if(path==='/voice.js')return new Response(Bun.file('apps/server/src/lab/voice.js'),{headers:{'content-type':'text/javascript;charset=utf-8'}});
 if(path==='/lab/api/speech'){
  const body=await req.json();assert.equal(body.stream,true);assert.equal(body.turn_id,'live-turn');requested=true;
  return new Response(new ReadableStream({start(controller){controller.enqueue(new Uint8Array(16384));},cancel(){cancelled=true;}}),{headers:{'content-type':'audio/pcm;rate=44100;channels=1','x-audio-turn-id':'live-turn','x-audio-encoding':'pcm_s16le'}});
 }
 return new Response(`<!doctype html><meta charset="utf-8"><div class="face"></div><div id="face-status"></div><div id="voice-status"></div><div id="audio-buffer"></div><div id="audio-format"></div><div id="first-audio">—</div><div id="speech-model"></div><input id="voice-enabled" type="checkbox"><button id="voice-stop">Stop</button><button id="voice-replay">Replay</button><script>let csrf='test',active='live-turn';const turns=new Map();function controls(){}function activity(){}function notice(message){window.lastError=message;}async function call(){active=null;}async function poll(){}</script><script src="/voice.js"></script>`,{headers:{'content-type':'text/html'}});
}});
const browser=await chromium.launch({headless:true});
try{
 const page=await browser.newPage();page.on('requestfinished',r=>{if(r.url().endsWith('/lab/api/speech'))ended=true;});await page.goto(server.url.toString());
 await page.locator('#voice-enabled').check();
 await page.evaluate(()=>{BuddyVoice.watch('live-turn');BuddyVoice.complete('live-turn');});
 try{await page.waitForFunction(()=>document.querySelector('#voice-status').textContent==='Speaking…',null,{timeout:10000});}catch(e){console.log(await page.evaluate(()=>({status:document.querySelector('#voice-status').textContent,error:window.lastError})));throw e;}
 assert(requested);assert.equal(ended,false);
 assert.equal(await page.locator('#first-audio').getAttribute('data-before-completion'),'true');
 await page.locator('#voice-stop').click();
 assert.equal(await page.locator('#voice-status').textContent(),'Voice stopped');
 for(let i=0;i<100&&!cancelled;i++)await new Promise(r=>setTimeout(r,20));
 assert(cancelled,'Stop must abort the open response');
 assert.equal(await page.evaluate(()=>window.lastError),undefined);
 console.log('PASS: browser plays PCM before response completion and Stop aborts the stream');
}finally{await browser.close();server.stop(true);}
