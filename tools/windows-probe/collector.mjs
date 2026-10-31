/** Read-only, same-origin metadata probe. No cookies, headers or novel text exported. */
export async function collectPixivTargets(targets, options = {}) {
  const report = {
    schema_version: 1, tool: 'open-pixiv-downloader-metadata-probe',
    started_at: new Date().toISOString(), status: 'running',
    scope: 'metadata_and_source_hashes_only', requests: 0, records: [],
    limitations: ['Media bytes, decoding and ZIP CRC are not checked.',
      'Windows installation and offline rendering are separate acceptance checks.'],
  };
  class Stop extends Error {
    constructor(code, status = null) { super(code); this.code = code; this.status = status; }
  }
  const interval = options.intervalMs ?? 3000;
  let lastStarted = 0;
  const types = { 0: 'illustration', 1: 'manga', 2: 'ugoira' };
  function safeURL(value) {
    if (typeof value !== 'string') return null;
    try {
      const parsed = new URL(value);
      if (parsed.protocol !== 'https:' || parsed.username || parsed.password) return null;
      return { host: parsed.hostname, path: parsed.pathname,
        query_parameter_names: [...new Set(parsed.searchParams.keys())] };
    } catch { return null; }
  }
  async function digest(value) {
    const bytes = new TextEncoder().encode(value);
    const hash = await crypto.subtle.digest('SHA-256', bytes);
    return { utf8_bytes: bytes.length,
      sha256: Array.from(new Uint8Array(hash), n => n.toString(16).padStart(2, '0')).join('') };
  }
  async function api(path) {
    if (!path.startsWith('/ajax/') || path.startsWith('//') || path.includes('#')) throw new Stop('invalid_api_path');
    if (report.requests >= 12) throw new Stop('request_budget_reached');
    const delay = Math.max(0, interval - (Date.now() - lastStarted));
    if (delay) await new Promise(resolve => setTimeout(resolve, delay));
    lastStarted = Date.now();
    report.requests++;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 20000);
    try {
      const response = await fetch('https://www.pixiv.net' + path, {
        method: 'GET', credentials: 'same-origin', redirect: 'error', cache: 'no-store',
        headers: { Accept: 'application/json' }, signal: controller.signal,
      });
      if (!response.ok) throw new Stop('http_error', response.status);
      if (!(response.headers.get('content-type') || '').toLowerCase().includes('json')) throw new Stop('expected_json');
      const reader = response.body.getReader();
      const chunks = [];
      let size = 0;
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          size += value.length;
          if (size > 8 * 1024 * 1024) { await reader.cancel(); throw new Stop('response_size_limit'); }
          chunks.push(value);
        }
      } finally { reader.releaseLock(); }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
      let envelope;
      try { envelope = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes)); }
      catch { throw new Stop('invalid_json'); }
      if (!envelope || typeof envelope !== 'object' || envelope.error !== false || !('body' in envelope)) throw new Stop('api_error_or_schema_changed');
      return envelope.body;
    } catch (error) {
      if (error instanceof Stop) throw error;
      throw new Stop(error?.name === 'AbortError' ? 'request_timeout' : 'request_failed_or_redirect');
    } finally { clearTimeout(timer); }
  }
  function count(source, pattern) { return [...source.matchAll(pattern)].length; }
  try {
    if (globalThis.location?.origin !== 'https://www.pixiv.net') throw new Stop('open_a_pixiv_page_first');
    if (!Array.isArray(targets) || targets.length !== 4 || targets.some(t =>
      !/^[1-9][0-9]{0,19}$/.test(t.id) || !['illustration', 'manga', 'novel', 'ugoira'].includes(t.type))) throw new Stop('invalid_targets');
    for (const target of targets) {
      const entry = { work_id: target.id, declared_type: target.type, status: 'not_checked' };
      report.records.push(entry);
      try {
        const body = await api(`/ajax/${target.type === 'novel' ? 'novel' : 'illust'}/${target.id}`);
        if (!body || typeof body !== 'object' || String(body.id) !== target.id) throw new Stop('work_identity_mismatch');
        entry.observed_type = target.type === 'novel' ? 'novel' : types[body.illustType] ?? 'unknown';
        entry.type_matches = entry.observed_type === target.type;
        entry.author_id = body.userId == null ? null : String(body.userId);
        entry.created_at = body.createDate ?? null;
        entry.updated_at = body.uploadDate ?? null;
        entry.access_flags = { xRestrict: body.xRestrict ?? null, restrict: body.restrict ?? null,
          isUnlisted: body.isUnlisted ?? null };
        if (target.type === 'novel') {
          if (typeof body.content !== 'string') throw new Stop('missing_novel_content');
          entry.source = await digest(body.content);
          entry.source.utf16_code_units = body.content.length;
          entry.declared_text_count = body.textCount ?? null;
          entry.markup_counts = {
            chapters: count(body.content, /\[chapter:/g), page_breaks: count(body.content, /\[newpage\]/g),
            ruby: count(body.content, /\[\[rb:/g), links: count(body.content, /\[\[jumpuri:/g),
            bold: count(body.content, /\[b:/g), italic: count(body.content, /\[i:/g),
            emphasis: count(body.content, /\[\[emphasismark:/g),
          };
          entry.cover = safeURL(body.coverUrl);
          entry.series = body.seriesNavData && typeof body.seriesNavData === 'object'
            ? { id: body.seriesNavData.seriesId ?? null, order: body.seriesNavData.order ?? null } : null;
          const uploaded = [...new Set([...body.content.matchAll(/\[uploadedimage:([0-9]+)\]/g)].map(m => m[1]))];
          const refs = [...new Set([...body.content.matchAll(/\[pixivimage:([0-9]+(?:-[0-9]+)?)\]/g)].map(m => m[1]))];
          entry.uploaded_images = uploaded.slice(0, 50).map(id => {
            const image = body.textEmbeddedImages?.[id];
            return { id, metadata_present: !!image, original: safeURL(image?.urls?.original) };
          });
          entry.uploaded_images_total = uploaded.length;
          entry.artwork_references_total = refs.length;
          entry.artwork_references = [];
          if (refs.length) {
            const selected = refs.slice(0, 20);
            const query = new URLSearchParams(selected.map(id => ['id[]', id]));
            const insertions = await api(`/ajax/novel/${target.id}/insert_illusts?${query}`);
            entry.artwork_references = selected.map(id => ({ id,
              visible: insertions?.[id]?.visible === true,
              original: safeURL(insertions?.[id]?.illust?.images?.original) }));
          }
          entry.reference_inventory_truncated = uploaded.length > 50 || refs.length > 20;
        } else if (entry.observed_type === 'ugoira') {
          const motion = await api(`/ajax/illust/${target.id}/ugoira_meta`);
          if (!motion || !Array.isArray(motion.frames) || !motion.frames.length) throw new Stop('invalid_frame_metadata');
          entry.source_zip = safeURL(motion.originalSrc ?? motion.src);
          entry.frames = motion.frames.map(frame => ({ file: frame.file, delay_ms: frame.delay }));
          entry.frame_count = entry.frames.length;
          entry.frame_metadata_valid = entry.frames.every(f => typeof f.file === 'string'
            && /^[^/\\:]+$/.test(f.file) && Number.isFinite(f.delay_ms) && f.delay_ms > 0)
            && new Set(entry.frames.map(f => f.file)).size === entry.frames.length;
          entry.zip_crc_status = 'not_checked';
        } else if (entry.observed_type === 'illustration' || entry.observed_type === 'manga') {
          const pages = await api(`/ajax/illust/${target.id}/pages`);
          if (!Array.isArray(pages)) throw new Stop('invalid_page_list');
          entry.declared_page_count = body.pageCount ?? null;
          entry.observed_page_count = pages.length;
          entry.page_count_matches = Number.isInteger(body.pageCount) && body.pageCount > 0 && body.pageCount === pages.length;
          entry.pages = pages.map((page, index) => ({ index, width: page.width ?? null,
            height: page.height ?? null, original: safeURL(page.urls?.original) }));
        } else { throw new Stop('unknown_artwork_type'); }
        entry.status = 'metadata_checked';
      } catch (error) {
        entry.status = 'blocked_or_failed';
        entry.reason = error instanceof Stop ? error.code : 'unexpected_schema';
        entry.http_status = error instanceof Stop ? error.status : null;
        throw error;
      }
    }
    report.status = 'metadata_collection_finished';
  } catch (error) {
    report.status = 'stopped';
    report.stop_reason = error instanceof Stop ? error.code : 'unexpected_schema';
    report.http_status = error instanceof Stop ? error.status : null;
  }
  report.finished_at = new Date().toISOString();
  return report;
}
