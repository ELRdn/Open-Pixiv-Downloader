const requests=new WeakMap<HTMLImageElement,object>();

export async function setRecordingImageSource(image:HTMLImageElement,src:string){
  if(image.getAttribute('src')===src)return;
  const request={};
  requests.set(image,request);
  image.style.visibility='hidden';
  image.setAttribute('aria-busy','true');
  image.src=src;
  try{
    await image.decode();
  }catch{
    // A failed image keeps the browser's native alt-text fallback.
  }finally{
    if(requests.get(image)===request){
      image.style.visibility='';
      image.removeAttribute('aria-busy');
      requests.delete(image);
    }
  }
}
