import test from 'node:test';
import assert from 'node:assert/strict';
import {setRecordingImageSource} from '../src/recording-image.ts';

function recordingImage(){
  const pending:Array<{resolve:()=>void;reject:(reason:Error)=>void}>=[];
  const attributes=new Map<string,string>();
  const image={
    src:'./recordings/hero.gif',
    style:{visibility:''},
    getAttribute(name:string){return name==='src'?this.src:attributes.get(name)??null;},
    setAttribute(name:string,value:string){attributes.set(name,value);},
    removeAttribute(name:string){attributes.delete(name);},
    decode(){return new Promise<void>((resolve,reject)=>pending.push({resolve,reject}));},
  };
  return {image:image as unknown as HTMLImageElement,pending};
}

test('a language change hides the old frame until the replacement is decoded',async()=>{
  const {image,pending}=recordingImage();
  const changing=setRecordingImageSource(image,'./recordings/hero-en.gif');
  assert.equal(image.style.visibility,'hidden');
  assert.equal(image.getAttribute('aria-busy'),'true');
  pending[0].resolve();await changing;
  assert.equal(image.style.visibility,'');
  assert.equal(image.getAttribute('aria-busy'),null);
  assert.equal(image.src,'./recordings/hero-en.gif');
});

test('rapid language changes cannot reveal a stale decoded frame',async()=>{
  const {image,pending}=recordingImage();
  const first=setRecordingImageSource(image,'./recordings/hero-en.gif');
  const second=setRecordingImageSource(image,'./recordings/hero.gif');
  const latest=setRecordingImageSource(image,'./recordings/hero-en.gif');
  pending[0].resolve();await first;
  assert.equal(image.style.visibility,'hidden');
  pending[1].reject(new Error('Source changed'));await second;
  assert.equal(image.style.visibility,'hidden');
  assert.equal(image.getAttribute('aria-busy'),'true');
  pending[2].resolve();await latest;
  assert.equal(image.style.visibility,'');
  assert.equal(image.src,'./recordings/hero-en.gif');
});

test('a failed replacement restores the native image fallback',async()=>{
  const {image,pending}=recordingImage();
  const changing=setRecordingImageSource(image,'./recordings/hero-en.gif');
  pending[0].reject(new Error('Image unavailable'));await changing;
  assert.equal(image.style.visibility,'');
  assert.equal(image.getAttribute('aria-busy'),null);
});

test('an unchanged recording is not reloaded or hidden',async()=>{
  const {image,pending}=recordingImage();
  await setRecordingImageSource(image,'./recordings/hero.gif');
  assert.equal(pending.length,0);
  assert.equal(image.style.visibility,'');
});
