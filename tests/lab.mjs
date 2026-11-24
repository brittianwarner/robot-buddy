// Opt-in: isolated test server and provisioned test identity only.
import assert from 'node:assert/strict';
import { openLab } from './lib/lab.mjs';
const { browser, context, page, errors, csrf, bot, url } = await openLab();
try {
 if(process.env.BUDDY_TEST_CLAIM==='1' && (await page.locator('#owner').textContent())!=='Registered') {
  await page.locator('#claim').click();
  await page.locator('#claim-qr').waitFor({state:'visible'});
  const claim=await context.newPage();await claim.goto(await page.locator('#claim-link').getAttribute('href'));
  await claim.locator('button').click();
  await claim.waitForURL('**/auth/callback**');
  await page.locator('#confirm').waitFor({state:'visible',timeout:15000});
  await page.locator('#confirm').click();
  await page.waitForFunction(()=>document.querySelector('#owner').textContent==='Registered');
  await claim.close();
 }
 // The curated catalog has 21 premade speakers (Will is the default for new bots).
 await page.waitForFunction(()=>document.querySelector('#voice-choice').options.length===21);
 const names=await page.locator('#voice-choice option').allTextContents();
 assert(names.includes('Will')&&names.includes('Sarah'),'catalog lists Will and Sarah');
 for(const legacy of ['Adrian','Delia','Sheila'])assert(!names.includes(legacy),legacy+' is a retired Fish name');
 assert((await page.locator('#active-prompt').textContent()).includes('Buddy'));
 await page.locator('#inspect-persona').click();
 await page.locator('#persona-dialog').waitFor({state:'visible'});
 assert((await page.locator('#persona-text').inputValue()).length>0);
 await page.locator('#persona-dialog button').click();
 if(process.env.BUDDY_TEST_PERSONAS==='1') {
  const action=async(command)=>{const r=await context.request.post(url+'/lab/api/action',{headers:{origin:url,'x-buddy-csrf':csrf},data:{target:'conversation',command}});assert.equal(r.status(),200);return r.json();};
  const send=async(text)=>{const before=await action({type:'snapshot'});const ids=new Set(before.turns.map(t=>t.id));await page.locator('#message').fill(text);await page.locator('#send').click();for(let i=0;i<120;i++){await page.waitForTimeout(1000);const snap=await action({type:'snapshot'});const turn=snap.turns.find(t=>!ids.has(t.id));if(turn&&!['queued','running'].includes(turn.status)){assert.equal(turn.status,'completed','persona turn failed');return;}}throw new Error('persona test timed out');};
  await send('Act like an Arnold Schwarzenegger-inspired action hero. Call this persona Action Buddy. Keep it playful and optimistic, customize your voice to match, and do not claim an exact celebrity voice clone.');
  const profile=await action({type:'get_persona',name:'Action Buddy',version:0});
  assert.equal((await action({type:'get_system_prompt'})).prompt,profile.prompt);
  assert.deepEqual((await action({type:'get_voice_config'})).config,profile.voice);
  await send('Switch back to saved Buddy version 1 with its saved voice settings.');
  const buddy=await action({type:'get_persona',name:'Buddy',version:1});
  assert.equal((await action({type:'get_system_prompt'})).prompt,buddy.prompt);
  assert.deepEqual((await action({type:'get_voice_config'})).config,buddy.voice);
  console.log('PASS: live agent created and activated named character+voice profile, then restored Buddy version 1');
 }
 if(process.env.BUDDY_TEST_SETTINGS==='1') {
  await page.locator('#message').fill('Please use Sarah as your voice, keeping your delivery style unchanged. Set your timezone to America/New_York and keep your location label unknown. Save both settings using your tools.');
  await page.locator('#send').click();
  await page.waitForFunction(()=>document.querySelector('#saved-voice').textContent.startsWith('Sarah') && document.querySelector('#robot-context').textContent.includes('America/New_York'),null,{timeout:90000});
  await page.waitForFunction(()=>!document.querySelector('#send').disabled,null,{timeout:90000});
  await page.reload();
  await page.waitForFunction(()=>document.querySelector('#saved-voice').textContent.startsWith('Sarah') && document.querySelector('#robot-context').textContent.includes('America/New_York'));
 }
 if(process.env.BUDDY_TEST_RULES==='1') {
  await page.locator('#message').fill('Add a speech rule that emphasizes the exact word blue. Preserve all my other voice settings.');
  await page.locator('#send').click();
  await page.waitForFunction(()=>document.querySelector('#voice-settings').textContent.includes('"text": "blue"'),null,{timeout:90000});
  await page.waitForFunction(()=>!document.querySelector('#send').disabled,null,{timeout:90000});
  await page.reload();
  await page.waitForFunction(()=>document.querySelector('#voice-settings').textContent.includes('"text": "blue"'));
 }
 if(process.env.BUDDY_TEST_VOICE==='1')await page.locator('#voice-enabled').check();
 await page.locator('#message').fill('Please remember my favorite color is blue.');
 await page.locator('#send').click();
 await page.waitForFunction(()=>document.querySelector('#activity').textContent.includes('Turn completed'),null,{timeout:90000});
 await page.waitForFunction(()=>document.querySelector('#memories').textContent.includes('blue'));
 if(process.env.BUDDY_TEST_VOICE==='1') {
  await page.waitForFunction(()=>document.querySelector('#voice-status').textContent==='Speaking…',null,{timeout:90000});
  assert.notEqual(await page.locator('#first-audio').textContent(),'—');
  await page.locator('#voice-stop').click();
  assert.equal(await page.locator('#voice-status').textContent(),'Voice stopped');
  await page.waitForTimeout(500);
  await page.locator('#voice-replay').click();
  await page.waitForFunction(()=>document.querySelector('#voice-status').textContent==='Voice ready',null,{timeout:90000});
 }

 await page.reload();
 await page.waitForFunction(()=>document.querySelector('#messages').textContent.includes('Please remember my favorite color'));
 assert.equal(await page.locator('#bot-id').textContent(),bot);
 assert.equal(await page.locator('#voice-enabled').isChecked(),false,'reload must not enable audio replay');
 const response=await context.request.get(url+'/lab/api/state');
 assert.equal(response.headers()['cache-control'],'no-store');
 await page.screenshot({path:process.env.BUDDY_TEST_SCREENSHOT||'/tmp/robot-buddy-lab.png',fullPage:true});
 await page.setViewportSize({width:390,height:844});
 assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true,'mobile overflow');
 await page.screenshot({path:'/tmp/robot-buddy-lab-mobile.png',fullPage:true});
 assert.deepEqual(errors,[]);
 console.log('PASS: login, origin/CSRF, command allowlist, first boot/reconnect, optional OIDC claim, Rig tool/chat, memory, optional PCM playback/stop, silent reload, mobile');
} finally {await browser.close();}
