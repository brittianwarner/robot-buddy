// Opt-in: isolated test server and provisioned test identity only.
import assert from 'node:assert/strict';
import { openLab } from './lib/lab.mjs';
const { browser, context, page, errors, csrf, bot, url } = await openLab();
try {
 await page.locator('#voice-enabled').check();
 let speechRequests=0;
 page.on('request',r=>{if(r.url().endsWith('/lab/api/speech')){assert.equal(r.postDataJSON().stream,true);speechRequests++;}});
 const started=Date.now();
 let firstText=null,speechRequest=null;
 page.on('request',r=>{if(r.url().endsWith('/lab/api/speech'))speechRequest=Date.now()-started;});
 page.on('response',async r=>{if(r.url().endsWith('/lab/api/action')){try{const v=await r.json();if(Array.isArray(v)&&v.some(e=>e.kind==='text.delta')&&firstText===null)firstText=Date.now()-started;}catch{}}});
 await page.locator('#message').fill('Hello Buddy! How are you today?');
 await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelector('#first-audio').textContent!=='—',null,{timeout:90000});
 console.log(JSON.stringify({first_text_ms:firstText,speech_request_ms:speechRequest,first_audio_ms:Date.now()-started,tts:await page.locator('#first-audio').textContent()}));
 await page.waitForFunction(()=>document.querySelector('#voice-status').textContent==='Voice ready',null,{timeout:90000});
 assert.deepEqual(errors,[]);
}finally{await browser.close();}
