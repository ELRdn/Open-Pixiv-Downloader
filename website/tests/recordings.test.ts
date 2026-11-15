import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';

test('actual terminal recordings are complete, chronological and safe to embed',()=>{
  for(const language of ['ja','en'])for(const scene of ['hero','batch','resume','outputs']){
    const name=`${scene}${language==='en'?'-en':''}`;
    const lines=readFileSync(new URL(`../public/recordings/${name}.cast`,import.meta.url),'utf8').trim().split('\n');
    const header=JSON.parse(lines.shift()!);assert.equal(header.version,2);assert.equal(header.width,120);assert.equal(header.height,36);
    assert.equal(header.env.SHELL,'opixiv 0.3.0-alpha.1');assert.equal(header.env.LANG,language);
    assert.equal(header.theme.palette.split(':').length,16);
    const gif=readFileSync(new URL(`../public/recordings/${name}.gif`,import.meta.url));
    assert.equal(gif.toString('ascii',0,6),'GIF89a');assert.ok(gif.readUInt16LE(6)>1000&&gif.readUInt16LE(8)>600);
    const loop=gif.indexOf(Buffer.from('NETSCAPE2.0'));assert.ok(loop>=0);assert.equal(gif.readUInt16LE(loop+13),0,'GIF must loop indefinitely');
    const png=readFileSync(new URL(`../public/recordings/${name}.png`,import.meta.url));assert.equal(png.toString('ascii',1,4),'PNG');
    let previous=-1;let stream='';
    for(const line of lines){const [time,kind,data]=JSON.parse(line);assert.ok(Number.isFinite(time)&&time>=previous);assert.equal(kind,'o');assert.equal(typeof data,'string');stream+=data;previous=time;}
    assert.ok(previous>=5&&previous<=10);assert.ok(stream.includes('DEMO'));assert.ok(stream.includes('art_1003')||stream.includes('user:9001'));
    assert.ok(stream.includes(language==='en'?'Settings':'1行に1URL'),'TUI language must match the recording');
    assert.ok(stream.includes('\u001b[38;2;81;225;232m'),'recording must retain the TUI cyan palette');
    assert.ok(!stream.includes('\u001b[?1049l'),'must not exit into the host shell');
    assert.ok(!/cookies?\.json|PHPSESSID|Bearer\s|pixiv_refresh_token/i.test(stream));
  }
});
