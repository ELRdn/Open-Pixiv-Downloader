import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { collectPixivTargets } from './collector.mjs';

const targets = [
  { type: 'illustration', id: '149564947' }, { type: 'manga', id: '130117333' },
  { type: 'novel', id: '29476682' }, { type: 'ugoira', id: '151427872' },
];
const text = '非公開の本文をレポートへ出力しない[[rb:小説>しょうせつ]][uploadedimage:7][pixivimage:8-2]';
function corpus() {
  const image = (id, type, pageCount) => ({ id, userId: '44', illustType: type, pageCount, xRestrict: 0 });
  return new Map([
    ['/ajax/illust/149564947', image('149564947', 0, 1)],
    ['/ajax/illust/149564947/pages', [{ width: 10, height: 20, urls: { original: 'https://i.pximg.net/image.png?token=MEDIA_SECRET' } }]],
    ['/ajax/illust/130117333', image('130117333', 1, 2)],
    ['/ajax/illust/130117333/pages', [{ urls: { original: 'https://i.pximg.net/1.png' } }, { urls: { original: 'https://i.pximg.net/2.png' } }]],
    ['/ajax/novel/29476682', { id: '29476682', content: text, csrfToken: 'CSRF_SECRET',
      seriesNavData: { seriesId: '55', order: 2 },
      textEmbeddedImages: { '7': { urls: { original: 'https://i.pximg.net/7.png' } } } }],
    ['/ajax/novel/29476682/insert_illusts?id%5B%5D=8-2', { '8-2': { visible: true, illust: { images: { original: 'https://i.pximg.net/8.png' } } } }],
    ['/ajax/illust/151427872', image('151427872', 2, 1)],
    ['/ajax/illust/151427872/ugoira_meta', { originalSrc: 'https://i.pximg.net/frames.zip', frames: [{ file: '0.png', delay: 80 }, { file: '1.png', delay: 125 }] }],
  ]);
}
async function run(routes, replace = {}) {
  const oldFetch = globalThis.fetch, oldLocation = globalThis.location;
  const calls = [];
  globalThis.location = { origin: 'https://www.pixiv.net' };
  globalThis.fetch = async (value, options) => {
    const url = new URL(value);
    calls.push({ url: String(value), options });
    assert.equal(url.origin, 'https://www.pixiv.net');
    const path = url.pathname + url.search;
    if (replace[path]) return replace[path]();
    assert.ok(routes.has(path), 'Unexpected request: ' + path);
    return Response.json({ error: false, body: routes.get(path) });
  };
  try { return { report: await collectPixivTargets(targets, { intervalMs: 0 }), calls }; }
  finally { globalThis.fetch = oldFetch; globalThis.location = oldLocation; }
}

test('collects four metadata cases, original text digest and exact frame delays; exports no secrets or text', async () => {
  const { report, calls } = await run(corpus());
  assert.equal(report.status, 'metadata_collection_finished');
  assert.equal(report.requests, 8);
  assert.equal(report.records.length, 4);
  assert.ok(report.records.every(record => record.status === 'metadata_checked'));
  assert.equal(report.records[1].page_count_matches, true);
  assert.equal(report.records[2].source.sha256, createHash('sha256').update(text).digest('hex'));
  assert.equal(report.records[2].source.utf8_bytes, Buffer.byteLength(text));
  assert.equal(report.records[2].markup_counts.ruby, 1);
  assert.equal(report.records[2].artwork_references[0].visible, true);
  assert.deepEqual(report.records[3].frames.map(f => f.delay_ms), [80, 125]);
  const serialized = JSON.stringify(report);
  for (const excluded of [text, 'MEDIA_SECRET', 'CSRF_SECRET']) assert.ok(!serialized.includes(excluded));
  for (const call of calls) {
    assert.equal(call.options.credentials, 'same-origin');
    assert.equal(call.options.redirect, 'error');
    assert.deepEqual(Object.keys(call.options.headers), ['Accept']);
  }
});

for (const status of [401, 403, 429]) {
  test(`HTTP ${status} stops before any subsequent work or media request`, async () => {
    const { report, calls } = await run(corpus(), {
      '/ajax/illust/149564947': () => new Response('never export this body', { status }),
    });
    assert.equal(report.status, 'stopped');
    assert.equal(report.http_status, status);
    assert.equal(report.records.length, 1);
    assert.equal(calls.length, 1);
    assert.ok(!JSON.stringify(report).includes('never export this body'));
  });
}

test('mismatching identity fails instead of attributing a different work to a supplied URL', async () => {
  const routes = corpus();
  routes.set('/ajax/illust/149564947', { id: '9', illustType: 0 });
  const { report, calls } = await run(routes);
  assert.equal(report.stop_reason, 'work_identity_mismatch');
  assert.equal(calls.length, 1);
});

test('page-count mismatch is reported without inventing a successful full archive', async () => {
  const routes = corpus();
  routes.get('/ajax/illust/130117333').pageCount = 3;
  const { report } = await run(routes);
  assert.equal(report.records[1].page_count_matches, false);
  assert.equal(report.records[1].declared_page_count, 3);
  assert.equal(report.records[1].observed_page_count, 2);
  assert.equal(report.scope, 'metadata_and_source_hashes_only');
});

test('HTML challenge response stops without interpreting it as metadata', async () => {
  const { report, calls } = await run(corpus(), {
    '/ajax/illust/149564947': () => new Response('<html>challenge</html>', { headers: { 'Content-Type': 'text/html' } }),
  });
  assert.equal(report.stop_reason, 'expected_json');
  assert.equal(calls.length, 1);
});

test('API error envelope stops without exporting the site error body', async () => {
  const { report } = await run(corpus(), {
    '/ajax/illust/149564947': () => Response.json({ error: true, message: 'PRIVATE_ERROR', body: null }),
  });
  assert.equal(report.stop_reason, 'api_error_or_schema_changed');
  assert.ok(!JSON.stringify(report).includes('PRIVATE_ERROR'));
});
