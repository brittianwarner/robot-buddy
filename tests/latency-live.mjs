// Opt-in: isolated test server and provisioned test identity only.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
const url=process.env.BUDDY_TEST_SERVER_URL;
const token=process.env.BUDDY_LAB_TOKEN;
const identity=process.env.BUDDY_TEST_IDENTITY_FILE;
assert(url&&token&&identity,'Set BUDDY_TEST_SERVER_URL, BUDDY_LAB_TOKEN, BUDDY_TEST_IDENTITY_FILE');
const browser=await chromium.launch({headless:true});
try {
 const context=await browser.newContext({viewport:{width:1440,height:1000}});
 const page=await context.newPage();
 const errors=[];page.on('pageerror',e=>errors.push(e.message));
 assert.equal((await context.request.get(url+'/lab/api/state')).status(),400);
 assert.equal((await context.request.post(url+'/lab/api/login',{headers:{origin:'https://attacker.invalid'},data:{token}})).status(),400);
 await page.goto(url+'/lab');
 await page.locator('#operator-token').fill(token);
 await page.locator('#login-form button').click();
 await page.locator('#workspace').waitFor({state:'visible'});
 const csrf=await page.evaluate(async()=> (await (await fetch('/lab/api/state')).json()).csrf);
 assert.equal((await context.request.post(url+'/lab/api/action',{headers:{origin:url},data:{target:'bot',command:{type:'revoke'}}})).status(),400);
 await page.locator('#identity-file').setInputFiles(identity);
 await page.locator('#enroll').click();
 await page.waitForFunction(()=>document.querySelector('#bot-id').textContent!=='—');
 const bot=await page.locator('#bot-id').textContent();
 assert.equal((await context.request.post(url+'/lab/api/action',{headers:{origin:url,'x-buddy-csrf':csrf},data:{target:'bot',command:{type:'revoke'}}})).status(),400);

 await page.locator('#enroll').click();
 await page.waitForTimeout(1000);
 assert.equal(await page.locator('#bot-id').textContent(),bot);
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
