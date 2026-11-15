import './style.css';

const REPO='https://github.com/ELRdn/Open-Pixiv-Downloader';
const OPIXIV_VERSION='0.3.0-alpha.1';
const WINDOWS_DOWNLOAD=`${REPO}/releases/download/v${OPIXIV_VERSION}/opixiv-v${OPIXIV_VERSION}-windows-x86_64.zip`;
const COMMAND=`cargo install --git ${REPO}.git --branch main --locked --force`;
const reducedMotion=matchMedia('(prefers-reduced-motion: reduce)');
let paused=reducedMotion.matches;
const ja={
  skip:'本文へ移動',features:'機能',install:'導入する',faq:'FAQ',
  title:'好きな作品を、まとめて手元に。',lead:'作者・タグ・複数URLから、イラストと小説をまとめて保存。',
  heroCaption:'Windows実TUI / 合成4作品の一覧表示と、保存済み状態からの再開',
  full:'録画データを見る ↗',expand:'原寸GIFを見る ↗',pause:'一時停止',play:'再生する',mobileHint:'TUIは原寸GIFでも確認できます。機能の画像はタップで拡大。',
  installTitle:'この1行から、はじめよう。',copy:'コピー',copied:'コピーしました',copyError:'コマンドを選択してコピーしてください',
  requirement:'Rust / Cargo が必要です。導入後は opixiv で起動。',guide:'詳しい導入手順 ↗',
  windows:'Windows x64版のZIPを展開し、opixiv.exeがあるフォルダーでPowerShellを開いて .\\opixiv.exe で起動。RustやPythonは不要です。',release:'Windows版をダウンロード ↗',
  featuresTitle:'指定する。保存する。つづける。',featuresLead:'使う道具は、ひとつのターミナル。',
  f1Title:'一人の作者から、まとめて。',f1Body:'作者・タグ・複数URLで対象を指定。F5で作品一覧を確認してから、まとめて保存できます。',f1Caption:'合成の作者 user:9001 から4作品を一覧表示する実録。',
  f2Title:'中断しても、一覧はそのまま。',f2Body:'F11で最新の対象一覧から再開。URLを入れ直さず、保存済みの作品をスキップできます。',f2Caption:'保存済みの合成4作品から再開。取得要求が0回であることを画面に表示。',
  f3Title:'保存したあとも、見つけやすく。',f3Body:'保存結果と出力先をTUIで確認。画像と小説本文の集約先も、F4から確認できます。',f3Caption:'合成アーカイブの画像・小説の保存先を確認する実録。',
  demoNotice:'すべて実アプリの端末出力です。合成データを使用し、Pixivからの実取得は記録していません。',
  compareTitle:'繰り返す操作を、ひとつの流れに。',manual:'ブラウザで手動保存',tool:'OpenPixiv',
  c1:'対象の指定',m1:'作品を1件ずつ開く',o1:'作者・タグ・複数URLからまとめて指定',
  c2:'保存する操作',m2:'保存操作を繰り返す',o2:'一覧を確認して一括で処理',
  c3:'保存先の確認',m3:'保存したファイルを探す',o3:'TUIの出力先から確認・開く',
  c4:'中断したあと',m4:'保存済みか確かめ直す',o4:'保存済みの対象一覧から再開',
  compareNote:'手動で保存する場合の一例です。処理速度や保存成功率を比較したものではありません。',
  openTitle:'オープンに、育てています。',starLabel:'GitHub Star',starDate:'2026-11-15 確認時点',license:'オープンソース',interfaces:'使い方を選べる',
  q1:'無料で使えますか？',a1:'はい。MITライセンスのオープンソースです。ソースコードと導入手順はGitHubで公開しています。',
  q2:'最初の保存までに何が必要？',a2:'WindowsまたはLinuxの配布版を展開して起動するか、Cargoから導入します。保存先と作者・タグ・URLを指定し、F5で一覧を確認、F11 → Enterで保存できます。初期言語は英語。F10のLanguageから日本語に切り替えられます。現在の取得対象は匿名で閲覧できる公開・全年齢作品です。',
  q3:'Windows版はダウンロードできますか？',a3:'はい。v0.3.0-alpha.1のWindows x64版ZIPを公開しています。展開してopixiv.exeを起動してください。ReleasesではLinux x64版とSHA256SUMSも配布しています。',
  q4:'録画は実際の画面ですか？',a4:'はい。Windows版0.3.0-alpha.1の実TUIを日本語・英語で別々にConPTY録画し、ページの言語に合わせたGIFを自動ループで表示しています。合成4作品の準備済みアーカイブを使い、一覧・再開・保存先の確認を記録しました。起動時の待ち時間は短縮しています。実Pixivからの新規メディア取得は収録していません。',
  final:'次の保存から、OpenPixivで。',github:'GitHubで見る ↗',footer:'独立したオープンソースプロジェクト。Pixiv公式サービスではありません。',
};
type Key=keyof typeof ja;
const en:Record<Key,string>={
  skip:'Skip to content',features:'Features',install:'Get started',faq:'FAQ',
  title:'Your favorites. Saved together.',lead:'Batch-save illustrations and novels from creators, tags, or multiple URLs.',
  heroCaption:'Actual Windows TUI / four synthetic works, listing and resuming an existing archive',
  full:'View recording data ↗',expand:'Full-size GIF ↗',pause:'Pause',play:'Play',mobileHint:'Open the full-size GIF for a closer look. Tap feature images to enlarge.',
  installTitle:'Start with one command.',copy:'Copy',copied:'Copied',copyError:'Select the command and copy it manually',
  requirement:'Requires Rust / Cargo. Then launch with opixiv.',guide:'Installation guide ↗',
  windows:'Extract the Windows x64 ZIP, open PowerShell beside opixiv.exe, and run .\\opixiv.exe. No Rust or Python required.',release:'Download for Windows ↗',
  featuresTitle:'Add. Save. Pick up later.',featuresLead:'One terminal for the whole workflow.',
  f1Title:'A creator’s works, together.',f1Body:'Specify creators, tags, or multiple URLs. Preview your target list with F5 before processing it in a batch.',f1Caption:'Actual recording: listing four works from synthetic creator user:9001.',
  f2Title:'Keep your place. Keep your list.',f2Body:'Resume the latest target list with F11. Skip saved works without entering the URLs again.',f2Caption:'Resuming four already-saved synthetic works. The TUI shows zero fetch requests.',
  f3Title:'Find what you saved.',f3Body:'See results and output paths in the TUI. Use F4 to inspect the collection folders for images and novel text.',f3Caption:'Actual recording: inspecting the image and novel folders of a synthetic archive.',
  demoNotice:'All recordings show actual app output with synthetic data. They do not record live fetching from Pixiv.',
  compareTitle:'Less repetition. One workflow.',manual:'Saving manually in a browser',tool:'OpenPixiv',
  c1:'Choose works',m1:'Open works one by one',o1:'Add creators, tags, or multiple URLs together',
  c2:'Save',m2:'Repeat the save action',o2:'Review the list, then process it in a batch',
  c3:'Find files',m3:'Look for saved files',o3:'Inspect and open output paths from the TUI',
  c4:'Pick up later',m4:'Check what you already saved',o4:'Resume the saved target list',
  compareNote:'An example of a manual workflow, not a benchmark of speed or success rate.',
  openTitle:'Built in the open.',starLabel:'GitHub Star',starDate:'Checked 2026-11-15',license:'Open source',interfaces:'Choose your interface',
  q1:'Is it free?',a1:'Yes. OpenPixiv is open source under the MIT license. Find the source and installation instructions on GitHub.',
  q2:'What do I need for my first save?',a2:'Extract a Windows or Linux release, or install with Cargo. Choose an output folder and add creators, tags, or URLs. Review targets with F5, then press F11 → Enter to save. The UI starts in English; F10 lets you switch language. Fetching is anonymous and limited to works confirmed to be public and all-ages.',
  q3:'Can I download a Windows executable?',a3:'Yes. The v0.3.0-alpha.1 Windows x64 ZIP is available now. Extract it and launch opixiv.exe. Releases also includes a Linux x64 archive and SHA256SUMS.',
  q4:'Are these real screen recordings?',a4:'Yes. The actual Windows TUI of version 0.3.0-alpha.1 was recorded separately in English and Japanese through ConPTY. Automatically looping GIFs match the page language. A prepared archive of four synthetic works is used to show listing, resuming, and output paths. Startup waiting time is shortened. These recordings do not show new media downloads from Pixiv.',
  final:'Make your next save with OpenPixiv.',github:'View on GitHub ↗',footer:'An independent open-source project. Not an official Pixiv service.',
};
let lang:'ja'|'en'=new URLSearchParams(location.search).get('lang')==='en'?'en':'ja';
const t=(k:Key)=>(lang==='ja'?ja:en)[k];
const tx=(k:Key,tag='span',cls='')=>`<${tag} class="${cls}" data-i18n="${k}">${t(k)}</${tag}>`;
const recordingPath=(name:string,format:string)=>`./recordings/${name}${lang==='en'?'-en':''}.${format}`;
const recording=(name:string,hero=false)=>`<figure class="terminal ${hero?'hero-terminal':''}"><div class="terminal-bar"><span><i></i>opixiv <b>— ${name==='hero'?'terminal':name}</b></span><span>v${OPIXIV_VERSION} / WINDOWS / DEMO</span></div><div class="terminal-scroll"><a class="recording-image-link" data-recording-link="${name}" data-recording-format="gif" href="${recordingPath(name,'gif')}" aria-label="View ${name} recording at full size"><img class="recording-image" data-recording-image="${name}" src="${recordingPath(name,paused?'png':'gif')}" alt="Actual opixiv ${OPIXIV_VERSION} ${lang.toUpperCase()} ${name} TUI recording" loading="${hero?'eager':'lazy'}" width="1073" height="651"></a></div><figcaption>${tx(hero?'heroCaption':`f${name==='batch'?1:name==='resume'?2:3}Caption` as Key)}<a data-recording-link="${name}" data-recording-format="cast" href="${recordingPath(name,'cast')}" data-i18n="full">${t('full')}</a>${hero?`<a data-recording-link="hero" data-recording-format="gif" href="${recordingPath('hero','gif')}" data-i18n="expand">${t('expand')}</a>`:''}</figcaption></figure>`;

document.querySelector('#app')!.innerHTML=`
<a class="skip" href="#main" data-i18n="skip">${t('skip')}</a>
<header class="nav wrap" id="top"><a class="brand" href="#top" aria-label="OpenPixiv home"><img src="./favicon.svg" alt="" width="23" height="23">openpixiv<span>_</span></a><nav aria-label="Main"><a href="#features" data-i18n="features">${t('features')}</a><a href="#install" data-i18n="install">${t('install')}</a><a href="#faq">FAQ</a></nav><div class="nav-right"><a class="github-pill" href="${REPO}">GitHub ↗</a><div class="languages" role="group" aria-label="Language"><button data-lang="ja" type="button" aria-pressed="${lang==='ja'}">JP</button><span>/</span><button data-lang="en" type="button" aria-pressed="${lang==='en'}">EN</button></div></div></header>
<main id="main"><section class="hero wrap"><div class="hero-eyebrow"><p class="eyebrow"><i></i> OPEN SOURCE. LOCAL ARCHIVE.</p><span>WINDOWS · RUST · CLI + TUI</span></div><h1 data-i18n="title">${t('title')}</h1>${tx('lead','p','hero-lead')}
<div class="hero-toolbar"><span><span class="live-dot"></span> ACTUAL TERMINAL RECORDING</span><button id="motion" type="button" aria-pressed="false" data-i18n="pause">${t('pause')}</button></div>${recording('hero',true)}${tx('mobileHint','p','mobile-hint')}</section>
<section class="install wrap" id="install" aria-labelledby="install-title"><div class="install-heading"><p class="eyebrow">01 / INSTALL</p><h2 id="install-title" data-i18n="installTitle">${t('installTitle')}</h2></div><div class="install-command"><div class="tabs" role="tablist" aria-label="Install method"><button id="cargo-tab" type="button" role="tab" aria-selected="true" aria-controls="cargo-panel" tabindex="0">Cargo</button><button id="windows-tab" type="button" role="tab" aria-selected="false" aria-controls="windows-panel" tabindex="-1">Windows</button><span>ALPHA / MIT</span></div><div id="cargo-panel" role="tabpanel" aria-labelledby="cargo-tab"><div class="command-row"><span aria-hidden="true">$</span><code id="install-code">${COMMAND}</code><button id="copy" type="button">${tx('copy')}<span aria-hidden="true">⧉</span></button></div>${tx('requirement','p','install-note')}</div><div id="windows-panel" role="tabpanel" aria-labelledby="windows-tab" hidden>${tx('windows','p')}<a href="${WINDOWS_DOWNLOAD}" data-i18n="release">${t('release')}</a></div><a class="install-guide" href="${REPO}#readme" data-i18n="guide">${t('guide')}</a><p id="copy-status" class="sr-only" role="status"></p></div></section>
<section class="features wrap" id="features" aria-labelledby="features-title"><div class="section-heading"><div><p class="eyebrow">02 / WORKFLOW</p><h2 id="features-title" data-i18n="featuresTitle">${t('featuresTitle')}</h2></div>${tx('featuresLead','p')}</div>${['batch','resume','outputs'].map((name,i)=>`<article class="feature-row"><div class="feature-copy"><span class="feature-number">0${i+1}</span><h3 data-i18n="f${i+1}Title">${t(`f${i+1}Title` as Key)}</h3><p data-i18n="f${i+1}Body">${t(`f${i+1}Body` as Key)}</p><span class="key-hint">${['F5 → Ctrl+S','F11 / Ctrl+R','F4 / F6 / F7'][i]}</span></div>${recording(name)}</article>`).join('')}${tx('demoNotice','p','recording-notice')}</section>
<section class="comparison wrap" aria-labelledby="compare-title"><p class="eyebrow">03 / LESS REPETITION</p><h2 id="compare-title" data-i18n="compareTitle">${t('compareTitle')}</h2><div class="table-scroll"><table><thead><tr><th></th><th data-i18n="manual">${t('manual')}</th><th data-i18n="tool">${t('tool')}</th></tr></thead><tbody>${[1,2,3,4].map(n=>`<tr><th scope="row" data-i18n="c${n}">${t(`c${n}` as Key)}</th><td data-i18n="m${n}">${t(`m${n}` as Key)}</td><td data-i18n="o${n}">${t(`o${n}` as Key)}</td></tr>`).join('')}</tbody></table></div>${tx('compareNote','p','comparison-note')}</section>
<section class="open wrap"><p class="eyebrow">04 / OPEN SOURCE</p><div class="open-heading"><h2 data-i18n="openTitle">${t('openTitle')}</h2><a href="${REPO}" data-i18n="github">${t('github')}</a></div><div class="facts"><a href="${REPO}/stargazers"><strong>1<span>★</span></strong>${tx('starLabel')}${tx('starDate','small')}</a><a href="${REPO}/blob/main/LICENSE"><strong>MIT</strong>${tx('license')}<small>LICENSE</small></a><a href="${REPO}#readme"><strong>CLI + TUI</strong>${tx('interfaces')}<small>ONE TOOL, TWO WAYS</small></a></div></section>
<section class="faq wrap" id="faq"><p class="eyebrow">05 / FAQ</p><h2>FAQ</h2>${[1,2,3,4].map(n=>`<details><summary>${tx(`q${n}` as Key)}<span aria-hidden="true">+</span></summary>${tx(`a${n}` as Key,'p')}</details>`).join('')}</section>
<section class="final wrap"><h2 data-i18n="final">${t('final')}</h2><a class="button" href="#install">${tx('install')}<span aria-hidden="true">↗</span></a><a class="text-link" href="${REPO}">Star on GitHub ↗</a></section></main>
<footer class="wrap"><a class="brand" href="#top">openpixiv_</a>${tx('footer','p')}<a href="${REPO}/blob/main/LICENSE">MIT ↗</a></footer>`;

const motion=document.querySelector<HTMLButtonElement>('#motion')!;
function updateMotion(){motion.textContent=t(paused?'play':'pause');motion.setAttribute('aria-pressed',String(paused));}
function syncMedia(){
  document.querySelectorAll<HTMLImageElement>('[data-recording-image]').forEach(image=>{
    const src=recordingPath(image.dataset.recordingImage!,paused||document.hidden?'png':'gif');
    if(image.getAttribute('src')!==src)image.src=src;
    image.alt=`Actual opixiv ${OPIXIV_VERSION} ${lang.toUpperCase()} ${image.dataset.recordingImage} TUI recording`;
  });
  document.querySelectorAll<HTMLAnchorElement>('[data-recording-link]').forEach(link=>{link.href=recordingPath(link.dataset.recordingLink!,link.dataset.recordingFormat!);});
  updateMotion();
}
function setLanguage(value:'ja'|'en'){
  lang=value;document.documentElement.lang=lang;document.title=`OpenPixiv — ${t('title')}`;
  document.querySelector('meta[name="description"]')!.setAttribute('content',t('lead'));
  document.querySelectorAll<HTMLElement>('[data-i18n]').forEach(el=>el.textContent=t(el.dataset.i18n as Key));
  document.querySelector('h1')!.innerHTML=lang==='ja'?'<span>好きな作品を、</span><span>まとめて手元に。</span>':'<span>Your favorites.</span> <span>Saved together.</span>';
  document.querySelectorAll<HTMLButtonElement>('[data-lang]').forEach(el=>el.setAttribute('aria-pressed',String(el.dataset.lang===lang)));
  document.querySelector<HTMLAnchorElement>('[data-i18n="guide"]')!.href=`${REPO}/blob/main/${lang==='ja'?'README.ja.md#導入する':'README.md#install'}`;
  const url=new URL(location.href);if(lang==='en')url.searchParams.set('lang','en');else url.searchParams.delete('lang');history.replaceState(null,'',url);syncMedia();
}
document.querySelectorAll<HTMLButtonElement>('[data-lang]').forEach(el=>el.addEventListener('click',()=>setLanguage(el.dataset.lang as 'ja'|'en')));
const tabs=[...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')];
function selectTab(selected:HTMLButtonElement){tabs.forEach(tab=>{const active=tab===selected;tab.setAttribute('aria-selected',String(active));tab.tabIndex=active?0:-1;document.getElementById(tab.getAttribute('aria-controls')!)!.hidden=!active;});}
tabs.forEach((tab,index)=>{tab.addEventListener('click',()=>selectTab(tab));tab.addEventListener('keydown',event=>{if(!['ArrowLeft','ArrowRight','Home','End'].includes(event.key))return;event.preventDefault();const next=tabs[event.key==='Home'?0:event.key==='End'?tabs.length-1:(index+1)%tabs.length];selectTab(next);next.focus();});});
document.querySelector('#copy')!.addEventListener('click',async()=>{const status=document.querySelector('#copy-status')!;try{await navigator.clipboard.writeText(COMMAND);document.querySelector('#copy [data-i18n]')!.textContent=t('copied');status.textContent=t('copied');}catch{const range=document.createRange();range.selectNodeContents(document.querySelector('#install-code')!);getSelection()?.removeAllRanges();getSelection()?.addRange(range);status.textContent=t('copyError');}});
motion.addEventListener('click',()=>{paused=!paused;syncMedia();});
reducedMotion.addEventListener('change',event=>{paused=event.matches;syncMedia();});
document.addEventListener('visibilitychange',syncMedia);
setLanguage(lang);
