#!/usr/bin/env node
// 看板 (MarumaSign) の共有ロジックの、入出力の試験ベクタ (JSON) を作る。
//
// 使い方 (TORO-API のルートで):
//   node tools/gen-sign-vectors.mjs <TORO-3D-WebMap のパス> [<TORO-WebMap のパス>] [--out <出力先>]
//
// - 1 つ目の引数: 正本 (app/bluemap/marumasign/{core,manifest,xml-head}.ts) があるリポジトリ
// - 2 つ目の引数 (省略できる): サーバー側の本体 (server/lib/marumasign/{allow,kinds,limits,net}.ts) があるリポジトリ。
//   origin/main の内容を git で取り出して使う (取り出せなければ作業ツリーのファイル)。省略すると、サーバー側の項目は出力しない
// - 出力先の既定: tests/vectors/marumasign-shared.json
//
// TypeScript は Node の型除去 (Node 22.18 以降は既定で有効。古い Node では `node --experimental-strip-types` で実行) で
// そのまま読み込む。Rust の試験 (tests/sign_vectors.rs) が、できた JSON を読んで同じ結果になることを確かめる。
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const args = process.argv.slice(2)
let out = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'tests', 'vectors', 'marumasign-shared.json')
const positional = []
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--out') out = resolve(args[++i])
  else positional.push(args[i])
}
if (positional.length < 1) {
  console.error('usage: node tools/gen-sign-vectors.mjs <TORO-3D-WebMap> [<TORO-WebMap>] [--out <file>]')
  process.exit(2)
}
const canonicalDir = join(resolve(positional[0]), 'app', 'bluemap', 'marumasign')
const webmap = positional[1] ? resolve(positional[1]) : null

// ---------------------------------------------------------------- 取り込み

const work = mkdtempSync(join(tmpdir(), 'sign-vectors-'))
writeFileSync(join(work, 'package.json'), '{"type":"module"}')
const sources = {}
function sha(text) {
  return createHash('sha256').update(text).digest('hex')
}
for (const f of ['core', 'manifest', 'xml-head']) {
  const src = join(canonicalDir, `${f}.ts`)
  copyFileSync(src, join(work, `${f}.ts`))
  sources[`${f}.ts`] = sha(readFileSync(src, 'utf8'))
}
const server = {}
if (webmap) {
  for (const f of ['allow', 'kinds', 'limits', 'net']) {
    const rel = `server/lib/marumasign/${f}.ts`
    let text
    try {
      text = execFileSync('git', ['-C', webmap, 'show', `origin/main:${rel}`], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 })
    }
    catch {
      text = readFileSync(join(webmap, rel), 'utf8')
    }
    writeFileSync(join(work, `${f}.ts`), text)
    sources[`${f}.ts`] = sha(text)
  }
}
const load = f => import(pathToFileURL(join(work, `${f}.ts`)).href)
const core = await load('core')
const manifest = await load('manifest')
const xmlHead = await load('xml-head')
if (webmap) {
  Object.assign(server, { allow: await load('allow'), kinds: await load('kinds'), limits: await load('limits'), net: await load('net') })
}

// ---------------------------------------------------------------- 入力の部品

/** 大きな入力は、JSON に展開した形で置かない (head + unit を count 回 (unit の {i} は番号) + tail) */
function expand(t) {
  if (typeof t === 'string') return t
  let s = t.head ?? ''
  for (let i = 0; i < t.count; i++) s += t.unit.replaceAll('{i}', String(i))
  return s + (t.tail ?? '')
}
const rep = (head, unit, count, tail = '') => ({ head, unit, count, tail })

/** 文字 (0xFF 以下) → 16 進 */
const hex = s => Buffer.from(s, 'latin1').toString('hex')
const bytesHex = (...b) => Buffer.from(b).toString('hex')
const toBytes = h => Buffer.from(h, 'hex')

// ---------------------------------------------------------------- extensionOf / safeUrl / withoutFragment

const extensionUrls = [
  'https://x.test/a.png', 'https://x.test/A.PNG', 'https://x.test/a.tar.gz', 'https://x.test/a.', 'https://x.test/.png',
  'https://x.test/a', 'https://x.test/', 'https://x.test', 'https://x.test/a.png?x=1.jpg', 'https://x.test/a.png#f.jpg',
  'https://x.test/dir.d/file', 'https://x.test/a%2Epng', 'https://x.test/%E3%81%82.PNG', 'not a url', '', ' ',
  'https://x.test/a.JpEg?foo', 'https://x.test/path/to/file.M3U8', 'https://x.test/a..png', 'ftp://x.test/a.png',
  'data:text/plain,x.png', `https://x.test/${'a'.repeat(5000)}.png`, `https://x.test/${'a'.repeat(5000)}`,
  'https://日本語.test/画像.PNG', 'HTTPS://X.TEST/A.GIF', 'https://x.test/a.png/', 'https://x.test/a.b/c',
  'https://x.test:8443/a.mp4', 'https://x.test/a.\u00DF', 'https://x.test/a.%2e', 'https://x.test/./a.WEBM',
  'https://x.test/b/../a.Ogg', 'https://x.test/a.png?', 'https://x.test/a.png#', 'https://x.test/?a.png', 'https://x.test//a.mp3',
  'https://x.test/a b.png', 'https://x.test/a\\b.png', 'https://u:p@x.test/a.PDF', 'file:///C:/a/b.png', 'blob:https://x.test/uuid.png',
  'https://x.test/a.SVGZ', 'https://x.test/a.k', 'https://x.test/.', 'https://x.test/..', 'https://x.test/a.gltf;v=1',
  '//x.test/a.png', '/a.png', 'a.png', 'https://[::1]/a.PNG', 'https://127.0.0.1/a.Png',
]

function safeUrlCase(value, httpsOnly) {
  return { value, httpsOnly, expect: core.safeUrl(value, httpsOnly) }
}
const safeUrlValues = [
  'https://example.com/a.png', 'http://example.com/a.png', 'HTTPS://EXAMPLE.COM/A', 'https://example.com', 'https://example.com/',
  'https://example.com:443/a', 'https://example.com:80/a', 'http://example.com:80/a', 'http://example.com:443/a', 'https://example.com:8080/a',
  'https://example.com:8443/a', 'https://example.com:8444/a', 'https://example.com:0/a', 'https://example.com:65535/a', 'https://example.com:65536/a',
  'https://example.com:/a', 'https://user@example.com/', 'https://user:pass@example.com/', 'https://:pass@example.com/',
  'https://localhost/a', 'https://LOCALHOST/a', 'https://a.localhost/a', 'https://a.local/a', 'https://a.internal/a', 'https://a.lan/a',
  'https://a.home.arpa/a', 'https://home.arpa/a', 'https://intranet/a', 'https://example/a', 'https://127.0.0.1/a', 'https://127.1/a',
  'https://0x7f.0.0.1/a', 'https://2130706433/a', 'https://017700000001/a', 'https://[::1]/a', 'https://[2001:db8::1]/a', 'https://1.2.3.4/a',
  'https://1.2.3.4.example.com/a', 'https://example.com./a', 'https://example..com/a', 'https://.example.com/a', 'https://-a.example.com/a',
  'https://exa_mple.com/a', 'https://exa mple.com/a', 'https://example.com/a b', 'https://example.com/a%20b', 'https://example.com/a"b',
  'https://example.com/a\'b', 'https://example.com/a<b', 'https://example.com/a>b', 'https://example.com/a\\b', 'https://example.com/a`b',
  'https://example.com/a\tb', 'https://example.com/a\nb', ' https://example.com/', 'https://example.com/ ', 'https://example.com/\u007f',
  'https://example.com/#frag', 'https://example.com/a?q=1#frag', 'https://example.com/a?q=あ', 'https://example.com/あ.png', 'https://日本語.example/画像.png',
  'https://xn--wgv71a119e.example/a', 'https://EXAMPLE.com:443/A?B=C#D', 'https://example.com/%zz', 'https://example.com/../a/./b', 'https:example.com/a',
  'https:/example.com/a', 'https:///example.com/a', 'https://example.com\\a', 'https://example.com@evil.com/', 'https://evil.com#@example.com/',
  'ftp://example.com/a', 'javascript:alert(1)', 'data:text/plain,hi', 'file:///etc/passwd', 'ws://example.com/', 'mailto:a@example.com', '//example.com/a',
  'example.com/a', '', 'https://', 'https://.', 'http://', `https://example.com/${'a'.repeat(1975)}`, `https://example.com/${'a'.repeat(1976)}`,
  `https://example.com/${'a'.repeat(1977)}`, `https://example.com/${'あ'.repeat(1000)}`, `https://example.com/${'あ'.repeat(1990)}`,
  `https://${'a'.repeat(63)}.example.com/`, `https://${'a'.repeat(64)}.example.com/`, 'https://a.b/', 'https://a.bc/', 'https://xn--/', 'https://ａ.example/a',
  'https://example.com/a\u0080', 'https://example.com/a\u2028b', 'https://EXAMPLE.COM./', 'https://example.com:443', 'https://example.com:+80/', 'https://[::ffff:1.2.3.4]/',
]
const safeUrlCases = []
for (const v of safeUrlValues) {
  safeUrlCases.push(safeUrlCase(v, true), safeUrlCase(v, false))
}
for (const v of [null, 1, true, {}, [], undefined]) {
  safeUrlCases.push({ value: v === undefined ? null : v, httpsOnly: true, expect: core.safeUrl(v, true) })
}

const withoutFragmentCases = ['', '#', 'a#b', 'https://x.test/a#b#c', 'https://x.test/a?q#', '#only', 'no hash', 'a%23b#c', 'あ#い'].map(value => ({ value, expect: core.withoutFragment(value) }))

// ---------------------------------------------------------------- skipXmlHead / startsAfterXmlHead

const MPD_TAG = /<MPD[\s>]/y
const SVG_TAG = /<svg[\s>]/iy
const patterns = { mpd: MPD_TAG, svg: SVG_TAG }
const headTexts = [
  '', ' ', '   \n\t  ', '<a>', '  <a>', '<?xml version="1.0"?><MPD>', '<?xml version="1.0"?>\n<MPD xmlns="x">', '<?XML version="1.0"?><svg>', '<?xml', '<?xml >',
  '<?xml version="1.0"?', '<!-- c --><svg>', '<!-- c -->  <!-- d -->\n<svg>', '<!-- unclosed <svg>', '<!--> <svg>', '<!---> <svg>', '<!----> <svg>', '<!-- a -- b --><svg>',
  '<!DOCTYPE svg><svg>', '<!doctype svg PUBLIC "x" "y"><svg>', '<!DocType html><html>', '<!DOCTYPE', '<!DOCTYPE svg', '<?xml version="1.0"?><!DOCTYPE svg><svg>',
  '<?xml version="1.0"?><!-- c --><!DOCTYPE svg><!-- d --><svg width="1">', '<!DOCTYPE svg><?xml version="1.0"?><svg>', '<svg>', '<SVG>', '<Svg viewBox="0 0 1 1">', '<svgx>', '<svg\t>',
  '<svg/>', '<MPD>', '<MPD >', '<MPDx>', '<mpd>', '\uFEFF<svg>', '\u00A0<svg>', '\u3000<svg>', '\u2028<svg>', '\u200B<svg>', '\u0085<svg>', '\u180E<svg>', '\u1680<svg>', '\u202F<svg>', '\u205F<svg>',
  '<!-- 日本語 --><svg>', '<!-- 😀 --><svg>', '😀<svg>', '日本語<?xml version="1.0"?><svg>', '<?xml version="1.0"?> 😀 <svg>', '<?xml version="1.0" encoding="UTF-8"?>\r\n<!-- あ -->\r\n<svg>',
  '<!-- a --><?xml version="1.0"?><svg>', '<?xml version="1.0"?><?xml version="1.0"?><svg>', '<!----><!----><!----><svg>',
  rep('', '<!-- {i} -->', 1000, '<svg>'), rep('', '<!--', 1000), rep('<?xml?>', '<!DOCTYPE a>', 200, '<MPD>'), rep('', ' ', 100000, '<svg>'),
  rep('<!--', 'a', 100000), rep('<!--', '-', 10000, '>'), rep('', '<!--x-->', 5000),
]
const skipXmlHeadCases = []
for (const t of headTexts) {
  for (const [decl, doctype] of [[false, false], [true, false], [false, true], [true, true]]) {
    skipXmlHeadCases.push({ text: t, decl, doctype, expect: xmlHead.skipXmlHead(expand(t), { decl, doctype }) })
  }
}
const startsAfterCases = []
for (const t of headTexts) {
  for (const name of Object.keys(patterns)) {
    for (const [decl, doctype] of [[false, false], [true, true]]) {
      startsAfterCases.push({ text: t, pattern: name, decl, doctype, expect: xmlHead.startsAfterXmlHead(expand(t), patterns[name], { decl, doctype }) })
    }
  }
}

// ---------------------------------------------------------------- manifestRefs / matchPattern

function refsJson(r) {
  if (!r) return null
  return {
    urls: r.urls,
    patterns: r.patterns.map(p => ({ parts: p.parts, slots: p.slots, ids: [...p.ids], idLengths: [...p.idLengths] })),
    playlists: r.playlists,
  }
}
function summaryOf(r) {
  const j = refsJson(r)
  const first = a => (a.length ? a[0] : null)
  const last = a => (a.length ? a[a.length - 1] : null)
  return {
    urlsCount: j.urls.length, urlsFirst: first(j.urls), urlsLast: last(j.urls),
    patternsCount: j.patterns.length, patternsFirst: first(j.patterns), patternsLast: last(j.patterns),
    playlistsCount: j.playlists.length, playlistsFirst: first(j.playlists), playlistsLast: last(j.playlists),
  }
}

const B = 'https://cdn.example.com/live/main.m3u8'
const manifestCases = []
function addManifest(text, base = B, probes = [], summary = false) {
  const t = expand(text)
  const r = manifest.manifestRefs(t, base)
  const c = { text, base }
  if (summary) c.expectSummary = r ? summaryOf(r) : null
  else c.expect = refsJson(r)
  if (probes.length && r) {
    c.probes = probes
    c.matches = probes.map(u => r.patterns.map(p => manifest.matchPattern(p, u)))
    c.edges = probes.map(u => r.patterns.map(p => manifest.patternEdgesMatch(p, u)))
  }
  manifestCases.push(c)
}

// ---- HLS
addManifest('#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4,\nseg1.ts\n#EXTINF:4,\nseg2.ts\n#EXT-X-ENDLIST\n')
addManifest('#EXTM3U\r\n#EXT-X-STREAM-INF:BANDWIDTH=1000\r\nlow/index.m3u8\r\n#EXT-X-STREAM-INF:BANDWIDTH=2000\r\nhttps://other.example.net/hi.m3u8\r\n')
addManifest('\uFEFF#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI="keys/k1.bin",IV=0x01\nseg.ts\n')
addManifest('  \n\n#EXTM3U\nseg.ts')
addManifest('#extm3u\nseg.ts')
addManifest('x#EXTM3U\nseg.ts')
addManifest('#EXTM3U\n#EXT-X-MAP:URI="init.mp4"\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",URI="audio/a.m3u8"\n#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1,URI="iframe.m3u8"\n#EXT-X-RENDITION-REPORT:URI="../other/live.m3u8",LAST-MSN=10\n#EXT-X-SESSION-KEY:METHOD=AES-128,URI="https://keys.example.org/k"\n#EXT-X-PART:DURATION=1,URI="part1.m4s"\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI="part2.m4s"\n')
addManifest('#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,uri="lower.m3u8"\n#EXT-X-MEDIA:TYPE=AUDIO,Server-Uri="https://s.example.org/x.m3u8"\n#EXT-X-KEY:URI=nonquoted\n#EXT-X-KEY:XURI="no.bin"\n#EXT-X-KEY:URI="a.bin",URI="b.bin"\n#EXT-X-KEY:URI="unclosed\n#EXT-X-KEY:URI=""\n')
addManifest('#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n\n  \n   v/index.m3u8   \nseg.ts#frag\nseg.ts#other\nseg.ts\n')
addManifest('#EXTM3U\ndata:text/plain,hi\nftp://x.example.com/a\nhttp://plain.example.com/a\nhttps://u:p@cred.example.com/a\nmailto:a@b.c\n//proto.example.com/a\n/abs/path.ts\n../up.ts\n?query.ts\n#notaurl\n')
addManifest(`#EXTM3U\n${'a'.repeat(3990)}.ts\n${'a'.repeat(4000)}\n${'a'.repeat(4001)}\n`)
addManifest('#EXTM3U\n  \u00A0seg-nbsp.ts\u00A0\n\u2028seg-ls.ts\n\uFEFFseg-bom.ts\n日本語.ts\nあ/い.ts?q=う\n')
addManifest('#EXTM3U')
addManifest('#EXTM3U\n')
addManifest('#EXTM3U\r\n\r\n\r\n')
addManifest('#EXTM3U\nseg.ts', 'https://cdn.example.com/a/b/c/d.m3u8?token=1#x')
addManifest('#EXTM3U\nseg.ts?t=2', 'https://cdn.example.com/a/b/c/d.m3u8?token=1#x')
addManifest('#EXTM3U\nseg.ts', 'not a base')
addManifest('#EXTM3U\nhttps://abs.example.com/x.ts', 'not a base')
addManifest('#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\na.m3u8\nb.ts\n#EXT-X-STREAM-INF:BANDWIDTH=2\n#EXTINF:4,\nc.m3u8\n')
addManifest(rep('#EXTM3U\n', '#EXTINF:4,\nseg-{i}.ts\n', 20010), B, [], true)
addManifest(rep('#EXTM3U\n', '#EXT-X-STREAM-INF:BANDWIDTH={i}\nv{i}.m3u8\n', 20010), B, [], true)
addManifest(rep('#EXTM3U\n', '\n', 250000, 'last.ts\n'), B, [], true)
addManifest(rep('#EXTM3U\n', '#c\n', 199990, 'a.ts\nb.ts\n'), B, [], true)
addManifest(rep('#EXTM3U\n', 'dup.ts\n', 1000), B)
addManifest(rep('#EXTM3U\n', '#EXT-X-KEY:URI="k{i}.bin"\n', 100), B)

// ---- DASH
const MPD = (inner, attrs = '') => `<?xml version="1.0"?>\n<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"${attrs}>${inner}</MPD>`
const dashBase = 'https://cdn.example.com/vod/stream/manifest.mpd?auth=1'
addManifest(MPD('<Period><AdaptationSet><Representation id="v1" bandwidth="1000"><SegmentTemplate media="v/$RepresentationID$/seg-$Number$.m4s" initialization="v/$RepresentationID$/init.mp4" startNumber="1"/></Representation></AdaptationSet></Period>'), dashBase, [
  'https://cdn.example.com/vod/stream/v/v1/seg-1.m4s', 'https://cdn.example.com/vod/stream/v/v1/seg-12345.m4s', 'https://cdn.example.com/vod/stream/v/v1/seg-.m4s', 'https://cdn.example.com/vod/stream/v/v2/seg-1.m4s',
  'https://cdn.example.com/vod/stream/v/v1/init.mp4', 'https://cdn.example.com/vod/stream/v/v1/seg-1.m4s?x=1', 'https://cdn.example.com/vod/stream/v/v1/seg-1x.m4s', 'https://cdn.example.com/vod/stream/v/v1/seg-123456789012345678901.m4s',
  'https://cdn.example.com/vod/stream/v/v1/seg-12345678901234567890.m4s', 'https://other.example.com/vod/stream/v/v1/seg-1.m4s', '', 'x',
])
addManifest(MPD('<BaseURL>https://base.example.org/root/</BaseURL><Period><BaseURL>p1/</BaseURL><AdaptationSet><SegmentTemplate media="$Number%05d$.m4s" initialization="init-$Bandwidth$.mp4" timescale="1"/><Representation id="a" bandwidth="1"/><Representation id="ab" bandwidth="2"/></AdaptationSet></Period>'), dashBase, [
  'https://base.example.org/root/p1/00001.m4s', 'https://base.example.org/root/p1/1.m4s', 'https://base.example.org/root/p1/init-128000.mp4', 'https://base.example.org/root/p1/init-.mp4', 'https://base.example.org/root/p1/0.m4s',
])
addManifest(MPD('<Period><AdaptationSet><SegmentTemplate media="$RepresentationID$/$Time$-$Number$.m4s"/><Representation id="a" /><Representation id="a-1" /><Representation id="a-1-2"/></AdaptationSet></Period>'), dashBase, [
  'https://cdn.example.com/vod/stream/a/1-2.m4s', 'https://cdn.example.com/vod/stream/a-1/1-2.m4s', 'https://cdn.example.com/vod/stream/a-1-2/1-2.m4s', 'https://cdn.example.com/vod/stream/a-1-2/3-4.m4s', 'https://cdn.example.com/vod/stream/b/1-2.m4s', 'https://cdn.example.com/vod/stream/a/1-.m4s',
])
addManifest(MPD('<Period><AdaptationSet><SegmentTemplate media="$RepresentationID$/$Number$/x"/></AdaptationSet></Period>'), dashBase, [
  'https://cdn.example.com/vod/stream/abc/12/x', 'https://cdn.example.com/vod/stream/a.b/12/x', 'https://cdn.example.com/vod/stream/a/b/12/x', 'https://cdn.example.com/vod/stream//12/x', 'https://cdn.example.com/vod/stream/a?b/12/x', 'https://cdn.example.com/vod/stream/a#b/12/x',
])
addManifest(MPD('<Period><AdaptationSet><SegmentTemplate media="seg-$Number$$Time$.m4s"/><SegmentTemplate media="p$$q-$Number$.m4s"/><SegmentTemplate media="$Number$.m4s"/><SegmentTemplate media="a$Number$-$Time$-$Bandwidth$-$SubNumber$-$RepresentationID$.m4s"/><SegmentTemplate media="a$Number$-$Time$-$Bandwidth$-$SubNumber$-$RepresentationID$-$Number$.m4s"/></AdaptationSet></Period>'), dashBase, [
  'https://cdn.example.com/vod/stream/seg-12.m4s', 'https://cdn.example.com/vod/stream/p$q-1.m4s', 'https://cdn.example.com/vod/stream/p%24q-1.m4s', 'https://cdn.example.com/vod/stream/9.m4s', 'https://cdn.example.com/vod/stream/a1-2-3-4-x.m4s',
])
addManifest(MPD('<Period><AdaptationSet><SegmentTemplate media="$Number$$Time$.m4s" initialization="$Number$"/><SegmentTemplate media="//evil.example.net/$Number$.m4s"/><SegmentTemplate media="https://cdn2.example.com/p/$Number$.m4s"/><SegmentTemplate media="$Number$/x.m4s"/><SegmentTemplate media="/$Number$.m4s"/><SegmentTemplate media="../$Number$.m4s"/><SegmentTemplate media="?q=$Number$"/></AdaptationSet></Period>'), dashBase, [
  'https://cdn.example.com/1.m4s', 'https://cdn.example.com/vod/1/x.m4s', 'https://cdn.example.com/vod/stream/1/x.m4s', 'https://cdn2.example.com/p/9.m4s', 'https://evil.example.net/1.m4s', 'https://cdn.example.com/vod/stream/manifest.mpd?q=1',
])
addManifest(MPD('<Period><AdaptationSet><SegmentList><Initialization sourceURL="init.mp4"/><SegmentURL media="s1.m4s" index="s1.idx"/><SegmentURL media="s2.m4s"/><RepresentationIndex sourceURL="rep.idx"/></SegmentList></AdaptationSet></Period>'))
addManifest(MPD('<Period><SegmentTemplate media="m-$Number$.m4s" index="i-$Number$.idx" initialization="i"/><Location>https://elsewhere.example.com/new.mpd</Location><PatchLocation>https://elsewhere.example.com/patch</PatchLocation></Period>'), dashBase, ['https://cdn.example.com/vod/stream/m-1.m4s', 'https://cdn.example.com/vod/stream/i-1.idx'])
addManifest(MPD('<mpd:Period xmlns:mpd="x"><mpd:BaseURL>http://insecure.example.com/base/</mpd:BaseURL><mpd:AdaptationSet><mpd:SegmentURL xlink:href="x" media="a.m4s"/></mpd:AdaptationSet></mpd:Period>'))
addManifest(MPD('<Period><BaseURL>a&amp;b/&#x41;&#66;&lt;&gt;&quot;&apos;&bogus;&#0;&#x110000;&#xD800;&#99999999999;&#xFFFFFFFFFFFF;</BaseURL><SegmentURL media="s.m4s"/></Period>'))
addManifest(MPD('<Period><BaseURL><![CDATA[cdata/&amp;/]]></BaseURL><SegmentURL media="s.m4s"/></Period>'))
addManifest(MPD('<Period><BaseURL>x/</BaseURL><!-- <SegmentURL media="comment.m4s"/> --><SegmentURL media="s&amp;t.m4s?a=1&amp;b=2"/><?pi <SegmentURL media="pi.m4s"/> ?></Period>'))
addManifest(MPD('<Period><BaseURL>a/</BaseURL><SegmentURL media="s.m4s"/></Period><Period><SegmentURL media="t.m4s"/></Period>'))
addManifest(MPD('<Period><BaseURL>a/<BaseURL>b/</BaseURL></BaseURL><SegmentURL media="s.m4s"/></Period>'))
addManifest(MPD('<Period><AdaptationSet><BaseURL>one/</BaseURL><BaseURL>two/</BaseURL><SegmentURL media="s.m4s"/></AdaptationSet></Period>'))
addManifest(MPD('<Period><Foo><BaseURL>in-foo/</BaseURL></Bar><SegmentURL media="s.m4s"/></Foo></Period>'))
addManifest(MPD('<Period><SegmentURL media=\'single.m4s\'/><SegmentURL media=unquoted.m4s/><SegmentURL media = "spaced.m4s" /><SegmentURL media="a.m4s"index="b.idx"/><SegmentURL  media="ok2.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s" media="b.m4s"/><SegmentURL 1media="c.m4s" media="d.m4s"/><SegmentURL _x="1" media="e.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="unterminated.m4s/><SegmentURL media="after.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><!-- unclosed <SegmentURL media="b.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><![CDATA[ unclosed <SegmentURL media="b.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><?pi unclosed <SegmentURL media="b.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><!DOCTYPE x <SegmentURL media="b.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><< <1 < <SegmentURL media="b.m4s"/></Period>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><Broken attr</Period><SegmentURL media="b.m4s"/>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><Broken attr="x"</Period><SegmentURL media="b.m4s"/>'))
addManifest(MPD('<Period><SegmentURL media="a.m4s"/><Broken a b c/><SegmentURL media="b.m4s"/></Period>'))
addManifest(`<?xml version="1.0"?><!DOCTYPE MPD [<!ENTITY a "b">]><MPD><SegmentURL media="a.m4s"/></MPD>`)
addManifest(`<!-- c --><!DOCTYPE MPD><MPD><SegmentURL media="a.m4s"/></MPD>`)
addManifest(`<!-- c -->\n<?xml version="1.0"?><MPD><SegmentURL media="a.m4s"/></MPD>`)
addManifest('<MPD><SegmentURL media="a.m4s"/></MPD>')
addManifest('<MPD>')
addManifest('<MPD\n><SegmentURL media="a.m4s"/>')
addManifest('<MPDX><SegmentURL media="a.m4s"/></MPDX>')
addManifest('<mpd><SegmentURL media="a.m4s"/></mpd>')
addManifest('<MPD', B)
addManifest('\uFEFF<MPD><SegmentURL media="a.m4s"/></MPD>')
addManifest('  \n <MPD><SegmentURL media="a.m4s"/></MPD>')
addManifest('<?xml version="1.0"?><svg><SegmentURL media="a.m4s"/></svg>')
addManifest(MPD(`<Period>${Array.from({ length: 70 }, (_, i) => `<a${i}>`).join('')}<SegmentURL media="deep.m4s"/>${Array.from({ length: 70 }, (_, i) => `</a${69 - i}>`).join('')}<SegmentURL media="after.m4s"/></Period>`))
addManifest(MPD(`<Period>${Array.from({ length: 62 }, (_, i) => `<a${i}>`).join('')}<SegmentURL media="deep.m4s"/>${Array.from({ length: 62 }, (_, i) => `</a${61 - i}>`).join('')}<SegmentURL media="after.m4s"/></Period>`))
addManifest(MPD(`<Period><SegmentURL ${Array.from({ length: 70 }, (_, i) => `z${i}="1"`).join(' ')} media="late.m4s"/><SegmentURL ${Array.from({ length: 63 }, (_, i) => `z${i}="1"`).join(' ')} media="edge.m4s"/></Period>`))
addManifest(MPD(`<Period>${Array.from({ length: 300 }, (_, i) => `<Representation id="r${i}"/>`).join('')}<SegmentTemplate media="$RepresentationID$/s-$Number$.m4s"/></Period>`), dashBase, [
  'https://cdn.example.com/vod/stream/r0/s-1.m4s', 'https://cdn.example.com/vod/stream/r255/s-1.m4s', 'https://cdn.example.com/vod/stream/r256/s-1.m4s', 'https://cdn.example.com/vod/stream/r299/s-1.m4s',
])
addManifest(MPD(`<Period><Representation id="${'x'.repeat(128)}"/><Representation id="${'y'.repeat(129)}"/><Representation id=""/><Representation/><Representation id="日本"/><SegmentTemplate media="$RepresentationID$/s.m4s"/></Period>`), dashBase, [
  `https://cdn.example.com/vod/stream/${'x'.repeat(128)}/s.m4s`, `https://cdn.example.com/vod/stream/${'y'.repeat(129)}/s.m4s`, 'https://cdn.example.com/vod/stream/%E6%97%A5%E6%9C%AC/s.m4s',
])
addManifest(MPD(`<Period>${Array.from({ length: 300 }, (_, i) => `<SegmentTemplate media="t${i}/$Number$.m4s"/>`).join('')}</Period>`), dashBase, [
  'https://cdn.example.com/vod/stream/t0/1.m4s', 'https://cdn.example.com/vod/stream/t255/1.m4s', 'https://cdn.example.com/vod/stream/t256/1.m4s',
])
addManifest(MPD(`<Period>${Array.from({ length: 300 }, (_, i) => `<SegmentURL media="u${i}.m4s"/>`).join('')}</Period>`), dashBase, [], true)
addManifest(rep('<MPD>', '<SegmentURL media="s{i}.m4s"/>', 20010, '</MPD>'), dashBase, [], true)
addManifest(rep('<MPD>', '<x/>', 100010, '<SegmentURL media="late.m4s"/></MPD>'), dashBase, [], true)
addManifest(rep('<MPD>', '<x/>', 199990, '<SegmentURL media="late.m4s"/></MPD>'), dashBase, [], true)
addManifest(rep('<MPD>', '<a>x</a> ', 40000, '<SegmentURL media="late.m4s"/></MPD>'), dashBase, [], true)
addManifest(rep('<MPD><BaseURL>', 'a', 9000, '</BaseURL><SegmentURL media="s.m4s"/></MPD>'), dashBase, [], true)
addManifest(rep('<MPD><BaseURL>', 'a', 8191, '</BaseURL><SegmentURL media="s.m4s"/></MPD>'), dashBase, [], true)
addManifest(rep('<MPD><BaseURL>', 'a', 8193, '</BaseURL><SegmentURL media="s.m4s"/></MPD>'), dashBase, [], true)
addManifest(MPD('<Period><SegmentTemplate media="a$Number%0d$.m4s"/><SegmentTemplate media="b$Number%3x$.m4s"/><SegmentTemplate media="c$Number%o$.m4s"/><SegmentTemplate media="d$Number%X$.m4s"/><SegmentTemplate media="e$Number%5s$.m4s"/><SegmentTemplate media="f$Number%$.m4s"/><SegmentTemplate media="g$Time%d$$Number%d$.m4s"/><SegmentTemplate media="h$number$.m4s"/><SegmentTemplate media="i$Number.m4s"/><SegmentTemplate media="j$Foo$.m4s"/></Period>'), dashBase, [
  'https://cdn.example.com/vod/stream/a1.m4s', 'https://cdn.example.com/vod/stream/b1.m4s', 'https://cdn.example.com/vod/stream/c1.m4s', 'https://cdn.example.com/vod/stream/d1.m4s', 'https://cdn.example.com/vod/stream/hx.m4s', 'https://cdn.example.com/vod/stream/j$Foo$.m4s', 'https://cdn.example.com/vod/stream/j%24Foo%24.m4s', 'https://cdn.example.com/vod/stream/i$Number.m4s',
])
addManifest(MPD('<Period><SegmentTemplate media="MSMARK0X-$Number$.m4s"/><SegmentTemplate media="MSMARK99X$Number$.m4s"/><SegmentTemplate media="MSMARKX$Number$"/><SegmentTemplate media="MSMARK1$Number$X"/></Period>'))
addManifest(MPD('<Period><SegmentTemplate media="p/$Number$.m4s"/><SegmentTemplate media="q/$Number$.m4s"/></Period>'), 'https://a.example.com:8443/x/y.mpd', ['https://a.example.com:8443/x/p/1.m4s', 'https://a.example.com/x/p/1.m4s'])
addManifest(MPD('<Period><SegmentTemplate media="$Number$.m4s"/></Period>'), 'https://a.example.com/', ['https://a.example.com/1.m4s', 'https://a.example.com/99999.m4s', 'https://a.example.com/１.m4s'])
addManifest(MPD('<Period><SegmentTemplate media="$Number$.m4s"/></Period>'), 'https://a.example.com', ['https://a.example.com/1.m4s'])
addManifest(MPD('<Period><SegmentTemplate media="x$Number$"/></Period>'), 'https://a.example.com/', ['https://a.example.com/x1', 'https://a.example.com/x', 'https://a.example.com/x12345678901234567890', 'https://a.example.com/x1234567890123456789'])
addManifest(MPD('<Period><SegmentTemplate media="x$Number$1"/></Period>'), 'https://a.example.com/', ['https://a.example.com/x1', 'https://a.example.com/x11', 'https://a.example.com/x21', 'https://a.example.com/x121', 'https://a.example.com/x1111'])
addManifest(MPD('<Period><SegmentTemplate media="x$Number$-$Number$"/></Period>'), 'https://a.example.com/', ['https://a.example.com/x1-1', 'https://a.example.com/x1-', 'https://a.example.com/x-1'])
addManifest(MPD('<Period><Representation id="1"/><Representation id="12"/><SegmentTemplate media="r-$RepresentationID$2.m4s"/></Period>'), 'https://a.example.com/', ['https://a.example.com/r-12.m4s', 'https://a.example.com/r-122.m4s', 'https://a.example.com/r-1.m4s', 'https://a.example.com/r-2.m4s'])
addManifest(MPD('<Period><SegmentTemplate media="r-$RepresentationID$.m4s"/></Period>'), 'https://a.example.com/', ['https://a.example.com/r-abc.m4s', 'https://a.example.com/r-.m4s', 'https://a.example.com/r-a/b.m4s', 'https://a.example.com/r-a?b.m4s'])

// ---- glTF
addManifest('{"asset":{"version":"2.0"},"buffers":[{"uri":"scene.bin","byteLength":1}],"images":[{"uri":"tex/a.png"},{"uri":"data:image/png;base64,AAAA"},{"uri":"DATA:text/plain,x"},{"bufferView":0},{"uri":5},{"uri":"https://cdn2.example.com/t.jpg#f"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('\uFEFF{"asset":{"version":"2.0"},"buffers":[{"uri":"scene.bin"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('  \n{"asset": {"version":"2.0"}, "buffers": "no", "images": [null, 1, "x", {"uri":"ok.png"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset":{"version":"2.0"}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset":', 'https://cdn.example.com/m/model.gltf')
addManifest('{"buffers":[{"uri":"a.bin"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset" : {}, "buffers":[{"uri":"a.bin"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset"\n:{}, "images":[{"uri":"a b.png"},{"uri":"%E3%81%82.png"},{"uri":"あ.png"},{"uri":""},{"uri":"   "}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('[{"asset":{}}]', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset":{},"buffers":[{"uri":"a.bin"},{"uri":"a.bin"},{"uri":"b.bin"}],"images":[{"uri":"a.bin"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset":{},"buffers":{"0":{"uri":"x.bin"}},"images":null}', 'https://cdn.example.com/m/model.gltf')
addManifest(rep('{"asset":{},"buffers":[', '{"uri":"b{i}.bin"},', 20010, '{"uri":"last.bin"}]}'), 'https://cdn.example.com/m/model.gltf', [], true)
addManifest('{"asset":{},"images":[{"uri":"a.png"}],"images":[{"uri":"dup.png"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset":{},"buffers":[{"uri":"\\u3042.bin"},{"uri":"a\\u0020b.bin"}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset":{},"buffers":[{"uri":"big.bin","byteLength":1e400}]}', 'https://cdn.example.com/m/model.gltf')
addManifest('{"asset":{},"buffers":[{"uri":"x.bin"}],}', 'https://cdn.example.com/m/model.gltf')
addManifest('{ // comment\n"asset":{}}', 'https://cdn.example.com/m/model.gltf')

// ---- どれでもない
for (const t of ['', ' ', 'hello', '<html></html>', '<svg></svg>', 'PK\u0003\u0004', '{}', '{"asset"}', '#EXTM3', '# EXTM3U', '#EXTM3U'.toLowerCase(), '[1,2]', '\u0000#EXTM3U', 'MPD', '<?xml version="1.0"?>', '<?xml version="1.0"?><!-- unclosed']) addManifest(t)

// ---------------------------------------------------------------- signUrls

const signUrlCases = []
if (server.allow) {
  const add = data => signUrlCases.push({ data, expect: [...server.allow.signUrls(data)] })
  const row = (image, link, extra = []) => [1, 2, 3, 4, 0, image, link, ...extra]
  add(null); add(1); add('x'); add([]); add({}); add({ format: 2, signs: [] }); add({ format: 1 }); add({ format: 1, signs: {} }); add({ format: 1, signs: [] })
  add({ format: '1', signs: [row('https://a.example.com/x.png', 'https://a.example.com/y')] })
  add({ format: 1, signs: [row('https://a.example.com/x.png', 'https://b.example.com/y')] })
  add({ format: 1, signs: [row('https://a.example.com/x.png#frag', 'https://a.example.com/x.png#other'), row('https://a.example.com/x.png', '')] })
  add({ format: 1, signs: [row('http://a.example.com/x.png', 'http://b.example.com/y')] })
  add({ format: 1, signs: [row('', ''), row('', 'https://only-link.example.com/'), row('https://only-image.example.com/', '')] })
  add({ format: 1, signs: [[1, 2, 3, 4, 0, 'https://short.example.com/a'], [1, 2, 3, 4, 0], 'str', 5, null, {}, [], row('https://ok.example.com/a', 'https://ok.example.com/b')] })
  add({ format: 1, signs: [row('HTTPS://UPPER.EXAMPLE.COM/A?B=C', 'https://example.com:443/p'), row('https://localhost/a', 'https://127.0.0.1/a'), row('https://a.local/', 'https://a.internal/')] })
  add({ format: 1, signs: [row(5, 6), row(null, undefined), row({}, []), row('https://sp ace.example.com/', 'https://x.example.com/a b')] })
  add({ format: 1, signs: [row('https://x.example.com/あ.png', 'https://日本語.example/画像.png'), row('https://e.example.com:8443/a', 'https://e.example.com:8444/a')] })
  add({ format: 1, signs: Array.from({ length: 50 }, (_, i) => row(`https://img.example.com/${i % 10}.png`, `https://link.example.com/${i}`, [1, 2, 3, 4, 5, 6, 7, 8])) })
  add({ format: 1, signs: [row('https://a.example.com/x.png', 'https://a.example.com/y', [0, 0, 0, 1, 1, 0, 0, 0])], extra: 1 })
}

// ---------------------------------------------------------------- sniff (と、サーバー側の classify / looksLikeHtml / cleanType)

const sniffBase = []
const addHead = (name, h, types = [''], exts = ['']) => sniffBase.push({ name, hex: h, types, exts })
const ascii = s => hex(s)
const pad = (h, n) => h + '00'.repeat(Math.max(0, n - h.length / 2))
const U8 = (...a) => bytesHex(...a)

addHead('png', U8(0x89) + ascii('PNG\r\n') + U8(0x1A, 0x0A), ['', 'image/png', 'text/html'], ['', 'png', 'jpg'])
addHead('gif', ascii('GIF89a') + '0100010000')
addHead('jpeg', U8(0xFF, 0xD8, 0xFF, 0xE0) + '00104a46494600')
addHead('webp', ascii('RIFF') + '24000000' + ascii('WEBPVP8 '))
addHead('ani', ascii('RIFF') + '24000000' + ascii('ACON'))
addHead('bmp', pad(ascii('BM') + '36000000', 18))
addHead('bmp short', ascii('BM') + '3600')
addHead('ico', U8(0, 0, 1, 0, 1, 0))
addHead('cur', U8(0, 0, 2, 0, 1, 0))
addHead('ico zero count', U8(0, 0, 1, 0, 0, 0))
addHead('icns', ascii('icns') + '00000010')
addHead('psd', ascii('8BPS') + '0001')
addHead('qoi', ascii('qoif') + '00000001')
addHead('dds', ascii('DDS ') + '7c000000')
addHead('ktx2', U8(0xAB) + ascii('KTX 20') + U8(0xBB, 0x0D, 0x0A, 0x1A, 0x0A))
addHead('tiff le', ascii('II') + U8(42, 0))
addHead('tiff be', ascii('MM') + U8(0, 42))
addHead('bigtiff le', ascii('II') + U8(43, 0))
addHead('bigtiff be', ascii('MM') + U8(0, 43))
addHead('farbfeld', ascii('farbfeld') + '00000001')
addHead('hdr radiance', ascii('#?RADIANCE\n'))
addHead('hdr rgbe', ascii('#?RGBE\n'))
addHead('exr', U8(0x76, 0x2F, 0x31, 0x01, 2, 0, 0, 0))
addHead('sgi', U8(0x01, 0xDA, 0, 1))
addHead('sgi rle', U8(0x01, 0xDA, 1, 1))
addHead('ras', U8(0x59, 0xA6, 0x6A, 0x95))
addHead('pcx', pad(U8(0x0A, 5, 1), 128))
addHead('pcx short', U8(0x0A, 5, 1))
addHead('pnm p6', ascii('P6\n1 1\n255\n'))
addHead('pnm p1 sp', ascii('P1 1 1\n'))
addHead('pnm p8', ascii('P8\n'))
addHead('pfm', ascii('Pf\n1 1\n-1\n'))
addHead('pfm color', ascii('PF\r1 1\r'))
addHead('m4a', '00000020' + ascii('ftypM4A ') + '00000000')
addHead('m4b', '00000020' + ascii('ftypM4B ') + '00000000')
addHead('mp4', '00000020' + ascii('ftypisom') + '00000200' + ascii('isomiso2avc1mp41'))
addHead('3gp', '00000014' + ascii('ftyp3gp4') + '00000000' + ascii('3gp4'))
addHead('avif', '00000020' + ascii('ftypavif') + '00000000' + ascii('avifmif1miaf'))
addHead('avif compat', '00000020' + ascii('ftypmif1') + '00000000' + ascii('avifmif1miaf'))
addHead('avis', '00000020' + ascii('ftypavis') + '00000000' + ascii('avifavismsf1'))
addHead('heic', '00000020' + ascii('ftypheic') + '00000000' + ascii('mif1heic'))
addHead('heix', '00000020' + ascii('ftypheix') + '00000000' + ascii('mif1'))
addHead('heic seq', '00000020' + ascii('ftypheic') + '00000000' + ascii('mif1msf1'))
addHead('hevc seq', '00000020' + ascii('ftypmsf1') + '00000000' + ascii('hevc'))
addHead('ftyp zero size', '00000000' + ascii('ftypavif') + '00000000' + ascii('avif'))
addHead('ftyp tiny size', '00000008' + ascii('ftypavif') + '00000000' + ascii('avif'))
addHead('ftyp huge size', 'ffffffff' + ascii('ftypmp42') + '00000000' + ascii('avif'))
addHead('ftyp short', '00000020' + ascii('ftyp'))
addHead('jxl codestream', U8(0xFF, 0x0A, 0, 0))
addHead('jxl box', U8(0, 0, 0, 0x0C) + ascii('JXL ') + '0d0a870a')
addHead('jp2', U8(0, 0, 0, 0x0C) + ascii('jP  ') + '0d0a870a')
addHead('j2k', U8(0xFF, 0x4F, 0xFF, 0x51, 0))
addHead('fits', ascii('SIMPLE  =                    T'))
addHead('dpx', ascii('SDPX') + '00000000')
addHead('dpx le', ascii('XPDS') + '00000000')
addHead('iff ilbm', ascii('FORM') + '00000100' + ascii('ILBM'))
addHead('iff pbm', ascii('FORM') + '00000100' + ascii('PBM '))
addHead('iff acbm', ascii('FORM') + '00000100' + ascii('ACBM'))
addHead('aiff', ascii('FORM') + '00000100' + ascii('AIFF'))
addHead('aifc', ascii('FORM') + '00000100' + ascii('AIFC'), ['', 'audio/aiff'], ['', 'aif'])
addHead('pdf', ascii('%PDF-1.7\n%'))
addHead('pdf late', ascii('junk'.repeat(100)) + ascii('%PDF-1.4'))
addHead('pdf too late', pad('', 1020) + ascii('%PDF-1.4'))
addHead('pdf edge', pad('', 1018) + ascii('%PDF-'))
addHead('pdf edge2', pad('', 1019) + ascii('%PDF-'))
addHead('hls', ascii('#EXTM3U\n#EXT-X-VERSION:3\n'), ['', 'application/vnd.apple.mpegurl'])
addHead('flv', ascii('FLV') + '0105000000090000000012')
addHead('flv v2', ascii('FLV') + '0205')
addHead('mkv', U8(0x1A, 0x45, 0xDF, 0xA3, 0x9F))
addHead('ogg audio', ascii('OggS') + '0002000000000000000001' + ascii('vorbis'), ['', 'video/ogg', 'audio/ogg'])
addHead('ogg theora', ascii('OggS') + pad('', 40) + ascii('theora'))
addHead('ogg theora late', ascii('OggS') + pad('', 500) + ascii('theora'))
addHead('wav', ascii('RIFF') + '24000000' + ascii('WAVEfmt '))
addHead('avi', ascii('RIFF') + '24000000' + ascii('AVI LIST'))
addHead('flac', ascii('fLaC') + '00000022')
addHead('id3', ascii('ID3') + '03000000')
addHead('mp3 frame', U8(0xFF, 0xFB, 0x90, 0x00), ['', 'audio/mpeg', 'video/mp4'], ['', 'mp3', 'aac', 'wav'])
addHead('mp3 frame short', U8(0xFF, 0xFB, 0x90), ['', 'audio/mpeg'], ['', 'mp3'])
addHead('ts', U8(0x47) + pad('', 187) + U8(0x47))
addHead('ts short', U8(0x47) + pad('', 100))
addHead('m2ts', pad('', 4) + U8(0x47) + pad('', 191) + U8(0x47))
addHead('asf', U8(0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C), ['', 'audio/x-ms-wma'], ['', 'wma'])
addHead('mpeg ps', U8(0, 0, 1, 0xBA, 0x44), ['', 'audio/mpeg'], ['', 'mp3'])
addHead('mpeg video', U8(0, 0, 1, 0xB3, 0x16))
addHead('rm', ascii('.RMF') + '00000012')
addHead('ra', ascii('.ra') + U8(0xFD) + '00040003')
addHead('amr', ascii('#!AMR\n'))
addHead('au', ascii('.snd') + '00000018')
addHead('caf', ascii('caff') + '00010000')
addHead('ape', ascii('MAC ') + '960f')
addHead('wv', ascii('wvpk') + '00000000')
addHead('mpc sv8', ascii('MPCK'))
addHead('mpc sv7', ascii('MP+') + '07')
addHead('tta', ascii('TTA1') + '0100')
addHead('voc', ascii('Creative Voice File') + '1a1a00')
addHead('dts', U8(0x7F, 0xFE, 0x80, 0x01, 0, 0))
addHead('ac3', U8(0x0B, 0x77, 0x01, 0x02), ['', 'audio/ac3', 'audio/eac3', 'application/octet-stream'], ['', 'ac3', 'eac3', 'mp3'])
addHead('mxf', U8(0x06, 0x0E, 0x2B, 0x34, 0x02))
addHead('y4m', ascii('YUV4MPEG2 W1 H1'))
addHead('nut', ascii('nut/multimedia container') + '00')
addHead('ivf', ascii('DKIF') + '00002000')
addHead('dv', U8(0x1F, 0x07, 0x00, 0x3F), ['', 'video/dv', 'video/mp4'], ['', 'dv'])
addHead('glb', ascii('glTF') + '02000000')
addHead('nbt be', U8(10, 0, 4) + ascii('Data') + U8(1, 0, 3) + ascii('abc'))
addHead('nbt be short', U8(10, 0, 0))
addHead('nbt be empty name', U8(10, 0, 0, 10, 0, 1) + ascii('a'))
addHead('nbt le', U8(10, 4, 0) + ascii('Data') + U8(1, 3, 0) + ascii('abc'))
addHead('nbt long name', U8(10, 0, 65) + ascii('a'.repeat(65)))
addHead('nbt bad char', U8(10, 0, 2, 0x01, 0x41, 1, 0, 1, 0x41))
addHead('nbt bad type', U8(10, 0, 1) + ascii('a') + U8(13, 0, 1) + ascii('b'))
addHead('nbt child zero', U8(10, 0, 1) + ascii('a') + U8(1, 0, 0) + ascii('b'))
addHead('nbt child long', U8(10, 0, 1) + ascii('a') + U8(1, 0, 65) + ascii('b'))
addHead('nbt exact', U8(10, 0, 1, 0x61, 1, 0, 1))
addHead('gzip', U8(0x1F, 0x8B, 8, 0, 0, 0, 0, 0), ['', 'application/gzip'], ['', 'svgz', 'schem', 'litematic', 'nbt', 'mcstructure', 'schematic', 'spz', 'gz', 'SVGZ'])
addHead('gzip method 9', U8(0x1F, 0x8B, 9, 0))
addHead('splat ply', ascii('ply\nformat binary_little_endian 1.0\nelement vertex 1\nproperty float x\nproperty float f_dc_0\nend_header\n'))
addHead('splat ply crlf', ascii('ply\r\nformat ascii 1.0\r\nelement vertex 1\r\nproperty float f_dc_0\r\nend_header\r\n'))
addHead('splat ply packed', ascii('ply\nformat binary_little_endian 1.0\nelement chunk 1\nproperty float min_x\nelement vertex 1\nproperty uint packed_position\nend_header\n'))
addHead('splat ply plain', ascii('ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nend_header\n1\n'))
addHead('splat ply after end', ascii('ply\nformat ascii 1.0\nproperty float x\nend_header\nproperty float f_dc_0\n'))
addHead('splat ply no end', ascii('ply\nformat ascii 1.0\nproperty float f_dc_0 \n'))
addHead('splat ply trailing', ascii('ply\nformat ascii 1.0\nproperty float f_dc_0   \t\nend_header\n'))
addHead('splat ply extra token', ascii('ply\nformat ascii 1.0\nproperty float f_dc_0 x\nend_header\n'))
addHead('splat ply prefix token', ascii('ply\nformat ascii 1.0\nproperty float xf_dc_0\nend_header\n'))
addHead('splat ply middle', ascii('ply\nformat ascii 1.0\nxx property float f_dc_0\nend_header\n'))
addHead('splat ply 2spaces', ascii('ply\nformat ascii 1.0\nproperty  list   uchar  packed_position\nend_header\n'))
addHead('splat ply nbsp', ascii('ply\nformat ascii 1.0\nproperty\u00A0float\u00A0f_dc_0\nend_header\n'))
addHead('splat ply cr only', ascii('ply\nformat ascii 1.0\rproperty float f_dc_0\rend_header\n'))
addHead('splat spz4', ascii('NGSP') + '04000000' + pad('', 24))
addHead('splat spz4 short', ascii('NGSP') + '04000000' + pad('', 20))
addHead('splat spz3', ascii('NGSP') + '03000000' + pad('', 24))
addHead('sog zip meta', ascii('PK') + U8(3, 4) + pad('', 22) + U8(9, 0, 0, 0) + ascii('meta.json') + ascii('{}'))
addHead('sog zip webp', ascii('PK') + U8(3, 4) + pad('', 22) + U8(14, 0, 0, 0) + ascii('dir/means.webp'))
addHead('sog zip other', ascii('PK') + U8(3, 4) + pad('', 22) + U8(5, 0, 0, 0) + ascii('a.txt'), ['', 'application/zip'], ['', 'sog', 'zip'])
addHead('sog zip short', ascii('PK') + U8(3, 4) + pad('', 10), ['', 'application/zip'], ['', 'sog'])
addHead('sog zip long name', ascii('PK') + U8(3, 4) + pad('', 22) + U8(0xFF, 0xFF, 0, 0) + ascii('meta.json'))
addHead('ksplat', U8(0, 1, 0, 0) + '01000000' + pad('', 4) + '10000000' + '10000000' + '0000' + '0000' + pad('', 24), ['', 'application/octet-stream'], ['', 'ksplat'])
addHead('ksplat bad mode', U8(0, 1, 0, 0) + '01000000' + pad('', 4) + '10000000' + '10000000' + '0300' + '0000' + pad('', 24), [''], ['ksplat'])
addHead('ksplat zero', U8(0, 1, 0, 0) + '01000000' + pad('', 4) + '00000000' + '00000000' + '0000' + '0000' + pad('', 24), [''], ['ksplat'])
addHead('ksplat many sections', U8(0, 1, 0, 0) + '01040000' + pad('', 4) + '10000000' + '10000000' + '0000' + '0000' + pad('', 24), [''], ['ksplat'])
addHead('ksplat negative', U8(0, 1, 0, 0) + '01000000' + pad('', 4) + 'ffffffff' + '10000000' + '0000' + '0000' + pad('', 24), [''], ['ksplat'])
addHead('splat plain', pad('', 32), ['', 'application/octet-stream'], ['', 'splat'])
addHead('splat nan', U8(0, 0, 0xC0, 0x7F) + pad('', 28), [''], ['splat'])
addHead('splat inf', U8(0, 0, 0x80, 0x7F) + pad('', 28), [''], ['splat'])
addHead('splat short', pad('', 31), [''], ['splat'])
addHead('sog meta json', ascii('{"version":2,"means":{"shape":[1,2],"files":["a.webp","b.webp"]}}'))
addHead('sog meta json bom', U8(0xEF, 0xBB, 0xBF) + ascii('{"means":{"files": ["a.webp"]}}'))
addHead('sog meta json ws', ascii(' \n\t{"means" : { "x":1, "files" : [ "a" ]}}'))
addHead('sog meta nested', ascii('{"means":{"a":{"files":["x"]}}}'))
addHead('sog meta after brace', ascii('{"means":{"a":1},"files":["x"]}'))
addHead('sog meta two means', ascii('{"means":{"a":{}}, "means":{"files":["x"]}}'))
addHead('sog meta no array', ascii('{"means":{"files":{}}}'))
addHead('gltf json', ascii('{"asset":{"version":"2.0"}}'))
addHead('gltf json bom', U8(0xEF, 0xBB, 0xBF) + ascii('{"asset":{"version":"2.0"}}'))
addHead('gltf json ws', ascii('\r\n  {"asset":{}}'))
addHead('json arr', ascii('[1,2,3]'))
addHead('svg', ascii('<svg xmlns="http://www.w3.org/2000/svg"></svg>'), ['', 'image/svg+xml', 'text/html'], ['', 'svg'])
addHead('svg xml', ascii('<?xml version="1.0"?>\n<!DOCTYPE svg PUBLIC "x" "y">\n<!-- c -->\n<svg>'))
addHead('svg bom', U8(0xEF, 0xBB, 0xBF) + ascii('<svg>'), ['', 'image/svg+xml'], ['', 'svg'])
addHead('svg upper', ascii('<SVG>'))
addHead('svg late', ascii('<?xml version="1.0"?>' + '<!-- ' + 'x'.repeat(1100) + ' -->' + '<svg>'), ['', 'image/svg+xml'], ['', 'svg'])
addHead('svg ext late', ascii('<!-- ' + 'x'.repeat(1100)), [''], ['svg'])
addHead('svg ext ws', ascii('   '), [''], ['svg'])
addHead('svg ext other', ascii('hello'), [''], ['svg'])
addHead('svg ext bom', U8(0xEF, 0xBB, 0xBF) + ascii('hello'), [''], ['svg'])
addHead('mpd', ascii('<?xml version="1.0"?>\n<MPD xmlns="x">'), ['', 'application/dash+xml'])
addHead('mpd bare', ascii('<MPD>'))
addHead('mpd ws', ascii('  \n<MPD\n>'))
addHead('mpd comment', ascii('<!-- c --><MPD>'))
addHead('mpd doctype', ascii('<!DOCTYPE MPD><MPD>'))
addHead('mpd lower', ascii('<mpd>'))
addHead('mpd x', ascii('<MPDX>'))
addHead('mpd decl bad', ascii('<?xml version="1.0" <MPD>'))
addHead('xpm', ascii('/* XPM */\nstatic char *a[] = {'))
addHead('xpm ws', ascii('/*  XPM  */\n'))
addHead('xpm bad', ascii('/* XPM*/\n'))
addHead('xbm', ascii('#define a_width 16\n#define a_height 16\n'), ['', 'image/x-xbitmap'], ['', 'xbm'])
addHead('xbm sp', ascii('  #define   a_width   16\n'), [''], ['xbm'])
addHead('xbm no digit', ascii('#define a_width x\n'), [''], ['xbm'])
addHead('xbm no width', ascii('#define a_height 16\n'), [''], ['xbm'])
addHead('xbm width tok', ascii('#define _width 16\n'), ['image/x-xbitmap'], [''])
addHead('xbm long tok', ascii('#define aa_widthx 16\n'), ['image/x-xbitmap'], [''])
addHead('xbm inner', ascii('#define a_width_width 16\n'), ['image/x-xbitmap'], [''])
addHead('html', ascii('<!DOCTYPE html><html><body>hi</body></html>'), ['', 'text/html', 'application/xhtml+xml'], ['', 'html', 'png'])
addHead('html tag only', ascii('<html>'), ['text/html', ''], ['png', ''])
addHead('html comment', ascii('<!-- c --><html>'))
addHead('html bom', U8(0xEF, 0xBB, 0xBF) + ascii('<html>'))
addHead('html utf16 bom', U8(0xFE, 0xFF) + ascii('<html>'))
addHead('html utf16le bom', U8(0xFF, 0xFE) + ascii('<html>'))
addHead('html ws', ascii('\n\r\t <HTML lang="ja">'))
addHead('html p', ascii('<p>hi</p>'))
addHead('html a', ascii('<a href="x">'))
addHead('html a slash', ascii('<a/>'))
addHead('html param', ascii('<param>'))
addHead('html pre', ascii('<pre>'))
addHead('html script', ascii('<script>alert(1)</script>'))
addHead('html doctype', ascii('<!doctype  html >'))
addHead('html doctype xml', ascii('<!DOCTYPE svg><svg>'))
addHead('html xml decl', ascii('<?xml version="1.0"?><html>'))
addHead('html late', ascii('<!-- ' + 'x'.repeat(1100) + ' --><html>'))
addHead('html ctrl', ascii('<head\u000B>'))
addHead('html nbsp', ascii('<body\u00A0>'))
addHead('html each', ascii('<iframe>'))
for (const tag of ['meta', 'title', 'style', 'link', 'frameset', 'form', 'div', 'head', 'body']) addHead(`html ${tag}`, ascii(`<${tag} x>`), ['', 'text/plain'], ['', 'bin'])
addHead('text', ascii('hello world'), ['', 'text/plain', 'application/octet-stream'], ['', 'ts', 'm4s', 'key', 'bin', 'vtt', 'mp4', 'TS'])
addHead('binary', pad(U8(0xDE, 0xAD, 0xBE, 0xEF), 64), ['', 'application/octet-stream', 'video/mp4', 'audio/mpeg', 'image/png', 'multipart/x-mixed-replace', 'application/x-mpegurl', 'application/dash+xml', 'video/x-flv', 'image/svg+xml', 'image/x-tga', 'image/vnd.wap.wbmp', 'image/x-xwindowdump'], ['', 'tga', 'icb', 'vda', 'vst', 'targa', 'wbmp', 'xwd', 'pgx', 'png', 'svg'])
addHead('empty', '', ['', 'video/mp4', 'text/html'], ['', 'png', 'svg', 'tga'])
addHead('xwd', U8(0, 0, 0, 0x65, 0, 0, 0, 7), ['', 'image/x-xwindowdump'], ['', 'xwd'])
addHead('xwd bad', U8(0, 0, 0, 0x65, 0, 0, 0, 6), ['', 'image/x-xwindowdump'], ['', 'xwd'])
addHead('pgx', ascii('PG ML + 8 1 1\n'), [''], ['', 'pgx'])
addHead('pgx bad', ascii('PX'), [''], ['pgx'])
addHead('tga', pad(U8(0, 0, 2), 18), ['', 'image/x-tga', 'image/x-targa'], ['', 'tga', 'targa', 'icb'])
addHead('wbmp', U8(0, 0, 8, 8), ['', 'image/vnd.wap.wbmp'], ['', 'wbmp'])

const sniffCases = []
const sniffTypes = new Set()
for (const b of sniffBase) {
  const full = toBytes(b.hex)
  const lengths = new Set([0, 1, 4, 8, 12, 31, 32, 128, 189, 1024, full.length - 1, full.length])
  for (const type of b.types) {
    for (const ext of b.exts) {
      for (const n of lengths) {
        if (n < 0 || n > full.length) continue
        // 全部の長さを全部の組に付けると多すぎるので、組の数が多いときは長さを絞る
        if ((b.types.length * b.exts.length > 4) && ![0, 4, 8, full.length].includes(n)) continue
        const head = full.subarray(0, n).toString('hex')
        const s = core.sniff(new Uint8Array(full.subarray(0, n)), type, ext)
        sniffCases.push({ name: b.name, head, type, ext, expect: s, whole: n === full.length })
        sniffTypes.add(type)
      }
    }
  }
}
// 大きな先頭 (ply のヘッダーの 64 KiB の上限。16 進を head + unit * count + tail の形で書く)
for (const [name, h, unit, count, tail] of [
  ['ply 70k header', 'ply\nformat ascii 1.0\n', 'comment x\n', 7000, 'property float f_dc_0\nend_header\n'],
  ['ply 64k edge', 'ply\nformat ascii 1.0\n', 'c', 65536 - 21 - 40, '\nproperty float f_dc_0\nend_header\n'],
  ['ply 64k edge+1', 'ply\nformat ascii 1.0\n', 'c', 65536 - 21 - 40 + 1, '\nproperty float f_dc_0\nend_header\n'],
  ['ply 64k no end', 'ply\nformat ascii 1.0\n', 'comment x\n', 7000, 'property float f_dc_0\n'],
]) {
  const full = Buffer.from(h + unit.repeat(count) + tail, 'latin1')
  sniffCases.push({ name, headParts: { head: hex(h), unit: hex(unit), count, tail: hex(tail) }, type: '', ext: '', expect: core.sniff(new Uint8Array(full), '', '') })
}

const classifyCases = []
const looksLikeHtmlCases = []
const cleanTypeCases = []
if (server.kinds) {
  for (const c of sniffCases) {
    if (!c.whole) continue
    const head = new Uint8Array(toBytes(c.head))
    for (const [child, foreign] of [[false, false], [true, false], [true, true]]) {
      // 組の数を絞る (拡張子が空の組と、拡張子つきの 1 つ目)
      if (c.head.length > 400 && child) continue
      classifyCases.push({ name: c.name, head: c.head, type: c.type, ext: c.ext, child, foreign, expect: server.kinds.classify(head, c.type, c.ext, child, foreign) })
    }
    looksLikeHtmlCases.push({ head: c.head, expect: server.kinds.looksLikeHtml(head) })
  }
  for (const v of ['image/png', 'IMAGE/PNG', 'text/html; charset=utf-8', ' text/plain ;x', '', 'png', 'a/b/c', '/png', 'image/', 'a'.repeat(64) + '/b', 'a'.repeat(65) + '/b', 'a/' + 'b'.repeat(64), 'a/' + 'b'.repeat(65),
    'application/vnd.apple.mpegurl', 'x/y+z', 'x/y.z-w_v', 'x/y z', 'x/y,z', '-a/b', 'a/-b', '1/2', 'multipart/x-mixed-replace; boundary=x', 'ＩＭＡＧＥ/ＰＮＧ', ';', 'a/b;c;d', 'a/b!#$&^_.+-']) {
    cleanTypeCases.push({ value: v, expect: server.kinds.cleanType(v) })
  }
  cleanTypeCases.push({ value: null, expect: server.kinds.cleanType(undefined) })
}

// ---------------------------------------------------------------- サーバー側: net / limits / allow

const blockedCases = []
const limitKeyCases = []
const checkUrlCases = []
const siteOfCases = []
if (server.net) {
  const ips = [
    '1.1.1.1', '8.8.8.8', '0.0.0.0', '0.1.2.3', '10.0.0.1', '10.255.255.255', '11.0.0.1', '127.0.0.1', '127.255.255.255', '100.64.0.1', '100.127.255.255', '100.63.255.255', '100.128.0.1', '169.254.169.254', '169.253.0.1',
    '172.15.255.255', '172.16.0.1', '172.31.255.255', '172.32.0.1', '192.168.0.1', '192.167.255.255', '192.0.0.1', '192.0.1.1', '192.0.2.1', '192.0.3.1', '192.88.99.1', '192.88.98.1', '198.17.255.255', '198.18.0.1', '198.19.255.255',
    '198.20.0.1', '198.51.100.1', '198.51.101.1', '203.0.113.1', '203.0.112.1', '223.255.255.255', '224.0.0.1', '239.255.255.255', '240.0.0.1', '255.255.255.255', '203.0.114.5', ' 8.8.8.8 ', '1.2.3', '1.2.3.4.5', '256.1.1.1', '01.1.1.1', '1.1.1.1\n',
    '::', '::1', '::2', '0:0:0:0:0:0:0:1', '::ffff:8.8.8.8', '::ffff:127.0.0.1', '::ffff:10.1.2.3', '::ffff:7f00:1', '::ffff:0808:0808', '::8.8.8.8', '::127.0.0.1', '::0.0.0.1', '::ffff:0:8.8.8.8', '::ffff:0:127.0.0.1', '0:0:0:0:ffff:0:808:808',
    '64:ff9b::8.8.8.8', '64:ff9b::127.0.0.1', '64:ff9b::808:808', '64:ff9b:1::1', '64:ff9b:0:1::1', '64:ff9b::1:0:0', '2002:0808:0808::1', '2002:7f00:0001::1', '2002:c0a8:0001::', '2002::', '2001::1', '2001:0:4136:e378:8000:63bf:3fff:fdd2',
    '2001:db8::1', '2001:db9::1', '2001:10::1', '2001:20::1', '2001:2f::1', '2001:30::1', '2001:f::1', '2001:4860:4860::8888', '2606:4700:4700::1111', '2400:cb00::1', '100::', '100::1', '100:0:0:1::', '101::', 'fc00::1', 'fd12:3456::1', 'fe00::1',
    'fe80::1', 'fe80::1%eth0', 'febf::1', 'fec0::1', 'feff::1', 'ff00::1', 'ff02::1', '3fff::1', '4000::1', '2000::1', '1000::1', '[::1]', '[2606:4700:4700::1111]', '2606:4700:4700:0:0:0:0:1111', '2606:4700:4700::1111%1', 'not an ip', '', 'localhost', 'example.com', '::g', ':::', '1::2::3', '12345::1',
    '::ffff:1.2.3.4.5', '2001:db8:0:0:0:0:0:0:1', '1:2:3:4:5:6:7:8', '1:2:3:4:5:6:1.2.3.4', '::1.2.3.4:5', '0000:0000:0000:0000:0000:ffff:7f00:0001', '::ffff:0:0', '::ffff:0.0.0.0', '::ffff:255.255.255.255', '::ffff:224.0.0.1',
  ]
  for (const ip of ips) blockedCases.push({ ip, expect: server.net.isBlockedAddress(ip) })
  for (const ip of ips) {
    const b = server.net.ipBytes(ip)
    // 数える単位 (limitKey) は、IP アドレスでないものも含めて
    limitKeyCases.push({ ip, expect: server.limits.limitKey(ip) })
    if (b === null) continue
  }
  for (const v of ['unknown', 'x'.repeat(100), ' padded ', '203.0.113.5', '::ffff:203.0.113.5', '2001:db8:1:2:3:4:5:6', '2001:db8:1:2::', '2001:db8:1:2:ffff::1', '2001:DB8:1:2::1', 'FE80::1']) {
    limitKeyCases.push({ ip: v, expect: server.limits.limitKey(v) })
  }
  const urls = [...safeUrlValues, 'https://example.com./', 'https://example..com/', 'https://a.example.com../', 'https://example.com.:443/a', 'https://a.b.c.d.example.com/path?query#frag', 'https://example.com/a#b']
  for (const v of urls) checkUrlCases.push({ value: v, expect: server.net.checkUrl(v) })
}
if (server.allow) {
  for (const u of [
    'https://example.com/a', 'https://www.example.com/a', 'https://a.b.example.com/a', 'https://example.co.jp/a', 'https://www.example.co.jp/a', 'https://a.b.example.co.jp/a', 'https://example.com.au/', 'https://x.example.com.au/',
    'https://user.github.io/a', 'https://a.user.github.io/a', 'https://github.io/a', 'https://x.pages.dev/', 'https://a.b.pages.dev/', 'https://x.y.z.workers.dev/', 'https://a.b.c.d.e.example.com/', 'https://localhost/', 'https://x.test/', 'https://a.x.test/',
    'https://xn--wgv71a119e.xn--wgv71a119e/', 'https://日本語.example.jp/', 'https://a.co.uk/', 'https://a.b.co.uk/', 'https://a.ab.cd.com/', 'https://a.b.ab/', 'https://a.bcd.ab/', 'https://a.bcde.ab/', 'https://my.app.netlify.app/', 'https://a.b.app.netlify.app/', 'https://x.web.app/', 'https://x.hf.space/', 'not a url', '', 'https://[::1]/', 'https://1.2.3.4/', 'https://a.repl.co/', 'https://x.y.replit.app/',
  ]) siteOfCases.push({ url: u, expect: server.allow.siteOf(u) })
}

// ---------------------------------------------------------------- 出力

for (const c of sniffCases) delete c.whole
const vectors = {
  description: '看板 (MarumaSign) の共有ロジックの入出力。tools/gen-sign-vectors.mjs が TypeScript から作る。手で編集しない。',
  notes: [
    '位置 (skipXmlHead の expect) は JavaScript の文字列の位置 (UTF-16 の単位)',
    '大きな入力は {head, unit, count, tail} (unit の {i} は 0 から数える番号) で書いてある',
    'expectSummary は、大きな結果の件数と最初・最後の値だけ',
    'head は先頭のバイト列の 16 進',
  ],
  sources,
  extensionOf: extensionUrls.map(url => ({ url, expect: core.extensionOf(url) })),
  safeUrl: safeUrlCases,
  withoutFragment: withoutFragmentCases,
  skipXmlHead: skipXmlHeadCases,
  startsAfterXmlHead: startsAfterCases,
  manifestRefs: manifestCases,
  sniff: sniffCases,
}
if (server.allow) {
  vectors.signUrls = signUrlCases
  vectors.siteOf = siteOfCases
}
if (server.kinds) {
  vectors.classify = classifyCases
  vectors.looksLikeHtml = looksLikeHtmlCases
  vectors.cleanType = cleanTypeCases
}
if (server.net) {
  vectors.isBlockedAddress = blockedCases
  vectors.limitKey = limitKeyCases
  vectors.checkUrl = checkUrlCases
}

mkdirSync(dirname(out), { recursive: true })
// 1 件を 1 行にする (差分が読めるように)
let text = '{\n'
const entries = Object.entries(vectors)
entries.forEach(([k, v], i) => {
  const comma = i < entries.length - 1 ? ',' : ''
  const key = JSON.stringify(k)
  if (Array.isArray(v) && v.length && typeof v[0] === 'object') text += `  ${key}: [\n${v.map(x => '    ' + JSON.stringify(x)).join(',\n')}\n  ]${comma}\n`
  else text += `  ${key}: ${JSON.stringify(v)}${comma}\n`
})
writeFileSync(out, text + '}\n')
rmSync(work, { recursive: true, force: true })
const counts = Object.fromEntries(Object.entries(vectors).filter(([, v]) => Array.isArray(v) && v.length && typeof v[0] === 'object').map(([k, v]) => [k, v.length]))
console.log('wrote', out)
console.log(counts)
