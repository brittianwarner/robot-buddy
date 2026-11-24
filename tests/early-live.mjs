// Opt-in: isolated test server and provisioned test identity only.
import assert from 'node:assert/strict';
import { openLab } from './lib/lab.mjs';
const { browser, context, page, errors, csrf, bot, url } = await openLab();
try {
 await page.locator('#voice-enabled').check();
 let speechRequests=0;
 page.on('request',r=>{if(r.url().endsWith('/lab/api/speech')){assert.equal(r.postDataJSON().stream,true);speechRequests++;}});
 await page.locator('#message').fill('Tell me a long original story about a cheerful robot exploring a forest. Write fifty numbered-free natural sentences, about twenty words each. Start immediately with the story, use full sentences, and continue to the end without asking questions.');
 await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelector('#first-audio').textContent!=='—',null,{timeout:90000});
 const early=await page.locator('#first-audio').getAttribute('data-before-completion');
 assert.equal(speechRequests,1,'one continuous client stream, not per-sentence fanout');
 await page.locator('#cancel').click();
 assert.equal(await page.locator('#voice-status').textContent(),'Voice stopped');
 await page.waitForTimeout(800);
 await page.reload();
 assert.equal(await page.locator('#voice-enabled').isChecked(),false);
 await page.waitForFunction(()=>document.querySelector('#bot-id').textContent!=='—');
 assert.equal(await page.locator('#first-audio').textContent(),'—');
 await page.locator('#voice-enabled').check();
 await page.locator('#message').fill('Say exactly these three sentences and nothing else: Hello there, human. My circuits are feeling cheerful today. Let us build something wonderful together.');
 await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelector('#first-audio').textContent!=='—',null,{timeout:90000});
 await page.waitForFunction(()=>document.querySelector('#voice-status').textContent==='Voice ready',null,{timeout:120000});
 assert.equal(speechRequests,2);
 await page.screenshot({path:'/private/tmp/buddy-early-speech.png',fullPage:true});
 assert.deepEqual(errors,[]);
 console.log(JSON.stringify({passed:true,speech_started_before_turn_completed:early==='true',client_speech_requests:speechRequests,cancel_and_silent_reload:true,complete_multisentence_playback:true}));
}finally{await browser.close();}
