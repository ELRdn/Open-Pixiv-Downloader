import { collectPixivTargets } from './collector.mjs';

const targets = [
  { type: 'illustration', id: '149564947' },
  { type: 'manga', id: '130117333' },
  { type: 'novel', id: '29476682' },
  { type: 'ugoira', id: '151427872' },
];
const button = document.querySelector('#start');
const status = document.querySelector('#status');
const save = document.querySelector('#save');
const tabId = Number(new URLSearchParams(location.search).get('tabId'));
let fileURL;

button.addEventListener('click', async () => {
  button.disabled = true;
  save.hidden = true;
  status.textContent = '確認中です。元のPixivタブとこのタブを開いたままお待ちください。';
  try {
    if (!Number.isInteger(tabId) || tabId <= 0) throw new Error('no_target_tab');
    const originalTab = await chrome.tabs.get(tabId);
    if (!originalTab.url || new URL(originalTab.url).origin !== 'https://www.pixiv.net') {
      throw new Error('no_pixiv_tab');
    }
    const results = await chrome.scripting.executeScript({
      target: { tabId }, world: 'ISOLATED', func: collectPixivTargets, args: [targets],
    });
    const report = results[0]?.result;
    if (!report || report.tool !== 'open-pixiv-downloader-metadata-probe') throw new Error('no_report');
    const json = JSON.stringify(report, null, 2);
    document.querySelector('#result').textContent = json;
    document.querySelector('#details').hidden = false;
    if (fileURL) URL.revokeObjectURL(fileURL);
    fileURL = URL.createObjectURL(new Blob([json + '\n'], { type: 'application/json' }));
    save.href = fileURL;
    save.hidden = false;
    const checked = report.records.filter(record => record.status === 'metadata_checked').length;
    status.textContent = report.status === 'metadata_collection_finished'
      ? `4作品のメタデータを確認しました。レポートを保存して共有してください。`
      : `${checked}作品を確認した時点で停止しました（${report.stop_reason}${report.http_status ? ' / HTTP ' + report.http_status : ''}）。停止したレポートも保存できます。`;
  } catch {
    status.textContent = 'Pixivのタブを確認できませんでした。通常どおりPixivのページを開き、そのページ上で拡張アイコンを押してやり直してください。';
  } finally { button.disabled = false; }
});

addEventListener('pagehide', () => { if (fileURL) URL.revokeObjectURL(fileURL); });
