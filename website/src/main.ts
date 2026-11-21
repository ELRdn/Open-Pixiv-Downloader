import './style.css';
import {en, ja, type Key} from './copy';
import {setRecordingImageSource} from './recording-image';

const REPO = 'https://github.com/ELRdn/Open-Pixiv-Downloader';
const VERSION = '0.3.0-alpha.1';
const RELEASE = `${REPO}/releases/tag/v${VERSION}`;
const COMMAND = `cargo install --git ${REPO}.git --branch main --locked --force`;
const scenes = ['batch', 'resume', 'outputs'] as const;
type Scene = 'hero' | typeof scenes[number];
type Language = 'en' | 'ja';
const locationLanguage = (): Language => new URLSearchParams(location.search).get('lang') === 'en' ? 'en' : 'ja';
let lang = locationLanguage();
let selectedScene: typeof scenes[number] = 'batch';
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
let paused = reducedMotion.matches;

const paths = {
  arrow: 'M4 12h15m-6-6 6 6-6 6', outward: 'M6 18 18 6M6 6h12v12',
  play: 'm8 5 11 7-11 7z', pause: 'M8 5v14m8-14v14', menu: 'M3 6h18M3 12h18M3 18h18',
  terminal: 'm4 6 6 6-6 6m9 0h7', download: 'M12 3v12m-5-5 5 5 5-5M4 16v4h16v-4',
  code: 'm8 6-6 6 6 6m8-12 6 6-6 6m-3-15-3 18', copy: 'M8 8h12v13H8zM16 8V3H3v13h5',
  expand: 'M4 10V4h6m4 0h6v6m0 4v6h-6m-4 0H4v-6',
  folder: 'M3 6h7l2 3h9v11H3z', layers: 'm2 8 10-5 10 5-10 5zM2 12l10 5 10-5M2 16l10 5 10-5',
  resume: 'M4 5v6h6M4.5 10a8 8 0 1 1 .5 6', list: 'M9 6h12M9 12h12M9 18h12M3 6h1M3 12h1M3 18h1',
  plus: 'M12 5v14M5 12h14', monitor: 'M3 4h18v13H3zM12 17v4m-5 0h10',
};
function icon(name: keyof typeof paths) {
  return `<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="${paths[name]}"/></svg>`;
}
const t = (key: Key) => (lang === 'ja' ? ja : en)[key];
const tx = (key: Key) => `<span data-i18n="${key}">${t(key)}</span>`;
const brand = `<a class="brand" href="#top"><img src="./favicon.svg" width="32" height="32" alt="">OpenPixiv</a>`;
const sectionLinks = `<a href="#features">${tx('features')}</a><a href="#install">${tx('install')}</a><a href="#faq">${tx('faq')}</a>`;
const github = `<a class="github" href="${REPO}">GitHub ${icon('outward')}</a>`;
const startButton = `<a class="button primary" href="#install">${tx('install')}${icon('arrow')}</a>`;
const readme = (section = '') => {
  const anchors: Record<string, [string, string]> = {install: ['導入する', 'install'], first: ['最初の保存まで', 'your-first-save']};
  const anchor = anchors[section];
  return `${REPO}/blob/main/${lang === 'ja' ? 'README.ja.md' : 'README.md'}${anchor ? '#' + anchor[lang === 'ja' ? 0 : 1] : ''}`;
};
const guide = (key: Key, section = '') => `<a class="text-link" data-guide="${section}" href="${readme(section)}">${tx(key)}${icon('outward')}</a>`;
const recordingPath = (scene: Scene, ext: string) => `./recordings/${scene}${lang === 'en' ? '-en' : ''}.${ext}`;
const captionKey = (scene: Scene): Key => `${scene}Caption`;

function player(scene: Scene) {
  const hero = scene === 'hero';
  return `<figure class="player" id="${hero ? 'hero-player' : 'scene-player'}">
    <div class="screen">
      <div class="screen-bar"><span class="window-dots" aria-hidden="true"><i></i><i></i><i></i></span><span>opixiv <b>/</b> <span data-filename>${scene}.cast</span></span><span class="screen-platform">Windows x64</span></div>
      <a class="viewport" data-full href="${recordingPath(scene,paused ? 'png' : 'gif')}">
        <img src="${recordingPath(scene,paused ? 'png' : 'gif')}" alt="${t(captionKey(scene))}" width="1073" height="651" loading="${hero ? 'eager' : 'lazy'}" fetchpriority="${hero ? 'high' : 'low'}">
      </a>
      <div class="player-controls">
        <button type="button" data-motion></button>
        <div><a data-full>${icon('expand')}<span data-size-label></span></a><a data-gif hidden>${tx('full')}</a><a data-cast href="${recordingPath(scene,'cast')}" download>${tx('cast')}${icon('download')}</a></div>
      </div>
    </div>
    <figcaption><span class="recording-tag">${tx('recording')}</span><span data-caption>${t(captionKey(scene))}</span></figcaption>
  </figure>`;
}

const platforms = [
  {id: 'windows', name: 'Windows x64', file: 'windows-x86_64.zip', command: '.\\opixiv.exe', hint: 'winStart', download: 'zip'},
  {id: 'linux', name: 'Linux x64', file: 'linux-x86_64.tar.gz', command: './opixiv', hint: 'linuxStart', download: 'tar'},
] as const;
const sceneKeys = {batch: 'F5', resume: 'F11 → Enter', outputs: 'F4'};
const sceneIcons = {batch: 'list', resume: 'resume', outputs: 'folder'} as const;

document.querySelector('#app')!.innerHTML = `
  <a class="skip" href="#main">${tx('skip')}</a>
  <header id="top"><div class="wrap header-inner">${brand}
    <nav class="desktop-nav" data-label="nav">${sectionLinks}</nav>
    <div class="header-tools">${github}
      <div class="languages" role="group" data-label="language"><button data-lang="ja" lang="ja">JP</button><button data-lang="en" lang="en">EN</button></div>
      <details class="mobile-menu"><summary aria-controls="mobile-nav">${icon('menu')}${tx('menu')}</summary>
        <nav id="mobile-nav" data-label="nav">${sectionLinks}${github}</nav>
      </details>
    </div>
  </div></header>
  <main id="main">
    <section class="hero"><div class="wrap hero-grid">
      <div class="hero-copy">
        <a class="release-link" href="${RELEASE}"><span class="version">v${VERSION}</span>${tx('release')}${icon('outward')}</a>
        <h1>${tx('title1')} ${tx('title2')}</h1><p class="hero-lead">${tx('lead')}</p>
        <div class="hero-actions">${startButton}<a class="button secondary" href="#features">${icon('play')}${tx('watch')}</a></div>
        <div class="hero-meta"><span>${icon('code')}${tx('mit')}</span><i></i><span>${icon('monitor')}Windows / Linux</span></div>
      </div>
      <div class="archive"><div class="sleeve" aria-hidden="true"><span>~/demo-archive</span></div>${player('hero')}</div>
    </div></section>
    <section class="wrap section install" id="install" aria-labelledby="install-title">
      <div class="section-head"><h2 id="install-title">${tx('quick')}</h2>${guide('guide','install')}</div>
      <div class="install-grid">
        <article class="install-card binary"><div class="card-title">${icon('download')}<h3>${tx('binary')}</h3></div><p>${tx('binaryReq')}</p>
          <div class="tabs" role="tablist" data-label="platform">${platforms.map((p,i) => `<button role="tab" id="${p.id}-tab" aria-controls="${p.id}-panel" aria-selected="${!i}" tabindex="${i ? -1 : 0}">${p.name}</button>`).join('')}</div>
          ${platforms.map((p,i) => `<div role="tabpanel" id="${p.id}-panel" aria-labelledby="${p.id}-tab" ${i ? 'hidden' : ''}>
            ${p.id === 'linux' ? `<p class="hint linux-requirements">${tx('linuxReq')} <a href="#cargo-install">${tx('sourceBuild')}</a></p>` : ''}
            <a class="download-link" href="${REPO}/releases/download/v${VERSION}/opixiv-v${VERSION}-${p.file}">${tx(p.download)}${icon('download')}</a>
            <p class="hint">${tx(p.hint)}</p><code>${p.command} --no-update-check</code>
          </div>`).join('')}
        </article>
        <article class="install-card" id="cargo-install"><div class="card-title">${icon('terminal')}<h3>${tx('cargo')}</h3></div><p>${tx('cargoReq')}</p>
          <div class="command"><div class="command-head"><span>Terminal</span><button id="copy">${icon('copy')}${tx('copy')}</button></div>
          <div class="command-code"><span aria-hidden="true">$</span><code id="install-code">${COMMAND}</code></div></div><p id="copy-status" role="status" class="hint"></p>
        </article>
      </div>
      <div class="first"><div><h3>${tx('first')}</h3><p>${tx('firstBody')}</p></div>
        <ol class="steps"><li><code>opixiv --no-update-check</code>${tx('launch')}</li><li><kbd>F2</kbd>${tx('demoKey')}</li><li><span><kbd>Ctrl+S</kbd><b> → </b><kbd>Enter</kbd></span>${tx('saveKey')}</li></ol>
        <div class="first-notes"><p>${tx('binaryHint')} ${tx('saved')} <code>demo-archive/</code></p>${guide('firstLink','first')}</div>
      </div>
    </section>
    <section class="demo section" id="features" aria-labelledby="features-title"><div class="wrap demo-grid">
      <div><h2 id="features-title">${tx('scenes')}</h2><p class="intro">${tx('sceneIntro')}</p>
        <div class="scene-picker">${scenes.map(scene => `<button data-scene="${scene}" aria-controls="scene-player" aria-pressed="${scene === selectedScene}">${icon(sceneIcons[scene])}${tx(scene)}<kbd>${sceneKeys[scene]}</kbd></button>`).join('')}</div>
      </div>
      <div>${player(selectedScene)}<p class="demo-note">${tx('demoNote')}</p></div>
    </div></section>
    <div class="wrap section evidence"><section aria-labelledby="compare-title"><h2 id="compare-title">${tx('compare')}</h2>
      <table><thead><tr><th scope="col">${tx('task')}</th><th scope="col">${tx('manual')}</th><th scope="col">OpenPixiv</th></tr></thead><tbody>
        ${(['choose','store','again'] as const).map((key,i) => `<tr><th scope="row">${tx(key)}</th><td><span class="cell-label" aria-hidden="true">${tx('manual')}</span>${tx(`m${i+1}` as Key)}</td><td><span class="cell-label" aria-hidden="true">OpenPixiv</span>${tx(`o${i+1}` as Key)}</td></tr>`).join('')}
      </tbody></table></section>
      <aside class="sources"><h3>${tx('source')}</h3><p>${tx('sourceNote')}</p>
        <a href="${REPO}">${tx('repository')}${icon('outward')}</a>
        <a href="${RELEASE}">${tx('checksums')}${icon('outward')}</a>
        <a href="${REPO}/tree/main/website#recording-provenance">${tx('provenance')}${icon('outward')}</a>
      </aside>
    </div>
    <section class="wrap section faq" id="faq"><h2>${tx('faq')}</h2><div>${[1,2,3,4,5].map(n => `<details><summary>${tx(`q${n}` as Key)}${icon('plus')}</summary><p>${tx(`a${n}` as Key)}</p></details>`).join('')}</div></section>
    <section class="wrap final"><div class="final-symbol" aria-hidden="true">${icon('layers')}</div><div><h2>${tx('final')}</h2><p>${tx('finalNote')}</p></div>${startButton}</section>
  </main>
  <footer><div class="wrap footer-inner">${brand}<nav data-label="footerNav">${github}<a href="${REPO}/blob/main/LICENSE">MIT</a>${guide('docs')}</nav><p>${tx('footer')}</p></div></footer>`;

function syncPlayers() {
  for (const [id, scene] of [['hero-player','hero'], ['scene-player',selectedScene]] as const) {
    const figure = document.getElementById(id)!;
    const image = figure.querySelector('img')!;
    void setRecordingImageSource(image, recordingPath(scene, paused || document.hidden ? 'png' : 'gif'));
    image.alt = t(captionKey(scene));
    figure.querySelector('[data-caption]')!.textContent = image.alt;
    figure.querySelector('[data-filename]')!.textContent = `${scene}${lang === 'en' ? '-en' : ''}.cast`;
    const sizeLabel = t(paused ? 'still' : 'full');
    figure.querySelector('[data-size-label]')!.textContent = sizeLabel;
    figure.querySelectorAll<HTMLAnchorElement>('[data-full]').forEach(link => {
      link.href = recordingPath(scene,paused ? 'png' : 'gif');
      link.setAttribute('aria-label', `${sizeLabel} · ${image.alt}`);
    });
    const gifLink = figure.querySelector<HTMLAnchorElement>('[data-gif]')!;
    gifLink.href = recordingPath(scene,'gif');
    gifLink.hidden = !paused;
    figure.querySelector<HTMLAnchorElement>('[data-cast]')!.href = recordingPath(scene,'cast');
    const motion = figure.querySelector<HTMLButtonElement>('[data-motion]')!;
    motion.innerHTML = icon(paused ? 'play' : 'pause') + t(paused ? 'play' : 'pause');
  }
}

function setLanguage(value: Language) {
  lang = value;
  document.documentElement.lang = lang;
  document.title = `OpenPixiv — ${t('title1')}${lang === 'en' ? ' ' : ''}${t('title2')}`;
  document.querySelector('meta[name="description"]')!.setAttribute('content', t('lead'));
  document.querySelectorAll<HTMLElement>('[data-i18n]').forEach(el => { el.textContent = t(el.dataset.i18n as Key); });
  document.querySelectorAll<HTMLElement>('[data-label]').forEach(el => el.setAttribute('aria-label', t(el.dataset.label as Key)));
  document.querySelectorAll<HTMLButtonElement>('[data-lang]').forEach(el => el.setAttribute('aria-pressed', String(el.dataset.lang === lang)));
  document.querySelectorAll<HTMLAnchorElement>('[data-guide]').forEach(el => { el.href = readme(el.dataset.guide); });
  syncPlayers();
}
document.querySelectorAll<HTMLButtonElement>('[data-lang]').forEach(button =>
  button.addEventListener('click', () => {
    const language = button.dataset.lang as Language;
    const url = new URL(location.href);
    if (language === 'en') url.searchParams.set('lang', 'en');
    else url.searchParams.delete('lang');
    history.replaceState(history.state, '', url);
    setLanguage(language);
  }));
window.addEventListener('popstate', () => {
  const language = locationLanguage();
  if (language !== lang) setLanguage(language);
});

const tabs = [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')];
function selectTab(selected: HTMLButtonElement) {
  for (const tab of tabs) {
    const active = tab === selected;
    tab.setAttribute('aria-selected', String(active));
    tab.tabIndex = active ? 0 : -1;
    document.getElementById(tab.getAttribute('aria-controls')!)!.hidden = !active;
  }
}
tabs.forEach((tab,index) => {
  tab.addEventListener('click', () => selectTab(tab));
  tab.addEventListener('keydown', event => {
    if (!['ArrowLeft','ArrowRight','Home','End'].includes(event.key)) return;
    event.preventDefault();
    const next = tabs[event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length-1 : 1-index];
    selectTab(next);
    next.focus();
  });
});

document.querySelector('#copy')!.addEventListener('click', async () => {
  const label = document.querySelector<HTMLElement>('#copy [data-i18n]')!;
  const status = document.querySelector<HTMLElement>('#copy-status')!;
  let message: Key = 'copyOk';
  try {
    await navigator.clipboard.writeText(COMMAND);
  } catch {
    message = 'copyError';
    const range = document.createRange();
    range.selectNodeContents(document.querySelector('#install-code')!);
    getSelection()?.removeAllRanges();
    getSelection()?.addRange(range);
  }
  label.dataset.i18n = message === 'copyOk' ? 'copied' : 'copy';
  label.textContent = t(label.dataset.i18n as Key);
  status.dataset.i18n = message;
  status.textContent = t(message);
});

const mobileMenu = document.querySelector<HTMLDetailsElement>('.mobile-menu')!;
mobileMenu.addEventListener('focusout', event => {
  if (!mobileMenu.contains(event.relatedTarget as Node | null)) mobileMenu.open = false;
});
mobileMenu.querySelectorAll('a').forEach(link => link.addEventListener('click', () => { mobileMenu.open = false; }));
document.addEventListener('keydown', event => {
  if (event.key === 'Escape' && mobileMenu.open) {
    mobileMenu.open = false;
    mobileMenu.querySelector('summary')!.focus();
  }
});

const sceneButtons = document.querySelectorAll<HTMLButtonElement>('[data-scene]');
sceneButtons.forEach(button => button.addEventListener('click', () => {
  selectedScene = button.dataset.scene as typeof selectedScene;
  sceneButtons.forEach(el => el.setAttribute('aria-pressed', String(el === button)));
  syncPlayers();
}));
document.querySelectorAll('[data-motion]').forEach(button => button.addEventListener('click', () => {
  paused = !paused;
  syncPlayers();
}));
reducedMotion.addEventListener('change', event => { paused = event.matches; syncPlayers(); });
document.addEventListener('visibilitychange', syncPlayers);
setLanguage(lang);
