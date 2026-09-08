// 시작 검사: 프런트 스크립트를 최상위부터 끝까지 실행해 도중에 던지는지 본다.
//
// 프런트가 최상위에서 한 번 던지면 창도 안 뜨고 세션 목록도 안 그려지는데,
// 컴파일도 통과하고 node --check도 통과해서 릴리스에 그대로 나간다(실제로 나갔다:
// setSidebarCollapsed가 선언 전의 profiles를 읽었다). 그 한 가지를 잡는다.
//
// DOM은 index.html에 실제로 있는 id/class만 존재하는 것으로 흉내 낸다 — 없는
// 요소를 만지는 코드도 여기서 걸린다. 실행 후 동작까지 검증하지는 않는다.
//
// 어떤 파일을 실행할지는 index.html의 <script> 순서에서 읽는다 — 파일을 나눠도
// 검사가 따라온다 (usage.js를 갈라낼 때 이 검사가 로드 순서 버그를 잡았다).
//
//   node tools/startup-check.js
const fs = require('fs');
const html = fs.readFileSync('ui/index.html', 'utf8');
const ids = new Set([...html.matchAll(/id="([^"]+)"/g)].map(m => m[1]));
const classes = new Set([...html.matchAll(/class="([^"]+)"/g)].flatMap(m => m[1].split(/\s+/)));

const mkEl = (name) => new Proxy({ __name: name }, {
  get(t, p) {
    if (p in t) return t[p];
    if (p === 'classList') return { add(){}, remove(){}, toggle(){}, contains(){return false} };
    if (p === 'style' || p === 'dataset') return {};
    if (p === 'querySelectorAll' || p === 'getElementsByClassName') return () => [];
    if (p === 'querySelector' || p === 'closest') return () => null;
    if (p === 'children') return [];
    if (p === 'value' || p === 'textContent' || p === 'innerHTML' || p === 'title') return '';
    if (p === 'checked' || p === 'disabled' || p === 'hidden') return false;
    if (typeof p === 'symbol') return undefined;
    return () => {};   // addEventListener, appendChild, focus, ...
  },
  set() { return true },
});

const sel = (s) => {
  if (typeof s !== 'string') return null;
  const m = s.match(/^#([\w-]+)$/);
  if (m) return ids.has(m[1]) ? mkEl(s) : null;
  const c = s.match(/^\.([\w-]+)$/);
  if (c) return classes.has(c[1]) ? mkEl(s) : null;
  return mkEl(s);   // 복합 선택자는 판단 불가 — 존재한다고 본다
};

global.window = {
  __TAURI__: {
    core: { invoke: () => Promise.resolve([]) },
    event: { listen: () => Promise.resolve(() => {}) },
    window: { getCurrentWindow: () => ({ show: async()=>{}, setFocus: async()=>{} }) },
    dialog: {}, clipboardManager: {}, process: {},
  },
  addEventListener(){}, removeEventListener(){},
  matchMedia: () => ({ matches: false, addEventListener(){} }),
};
// 부팅 경로가 둘이다: 문서가 아직 로딩 중이면 DOMContentLoaded를 기다리고,
// 이미 끝났으면 바로 시작한다. 두 경로를 각각 한 번씩 돌린다 (STARTUP_READY).
const readyState = process.env.STARTUP_READY === 'loading' ? 'loading' : 'complete';
const domReadyListeners = [];
global.document = {
  readyState,
  querySelector: sel,
  querySelectorAll: () => [],
  getElementById: (id) => (ids.has(id) ? mkEl('#'+id) : null),
  createElement: () => mkEl('created'),
  addEventListener(type, fn){ if (type === 'DOMContentLoaded' && typeof fn === 'function') domReadyListeners.push(fn); },
  removeEventListener(){},
  body: mkEl('body'), documentElement: mkEl('html'),
  hidden: false,
};
global.localStorage = { getItem: () => null, setItem(){}, removeItem(){} };
global.performance = { now: () => 0 };
global.PerformanceObserver = class { observe(){} };
global.ResizeObserver = class { observe(){} disconnect(){} };
global.MutationObserver = class { observe(){} disconnect(){} };
global.atob = (b) => Buffer.from(b, 'base64').toString('binary');
global.Terminal = class { constructor(){ this.unicode = {}; this.buffer={active:{}} } loadAddon(){} open(){} write(){} onData(){} onResize(){} onTitleChange(){} focus(){} dispose(){} attachCustomKeyEventHandler(){} };
global.FitAddon = { FitAddon: class { fit(){} } };
global.Unicode11Addon = { Unicode11Addon: class {} };
global.WebglAddon = { WebglAddon: class { onContextLoss(){} dispose(){} } };
global.navigator = { userAgent: 'probe' };
Object.assign(global, { setInterval: () => 0, setTimeout: () => 0 });

// index.html에 적힌 순서대로, vendor를 뺀 우리 스크립트만
const scripts = [...html.matchAll(/<script src="([^"]+)"><\/script>/g)]
  .map(m => m[1])
  .filter(src => !src.startsWith('vendor/'));
if (!scripts.length) {
  console.error('index.html에서 스크립트를 못 찾음');
  process.exit(1);
}
console.log('실행:', scripts.join(', '));

// 실제 페이지에서 여러 <script>는 같은 전역을 공유한다. 한 함수 본문에 이어 붙이면
// 같은 스코프가 되어 그 관계가 재현된다 — 앞 파일이 뒤 파일의 함수를 최상위에서
// 부르는 실수는 여기서 걸리지 않지만, readyState를 'complete'로 둬서 지연 실행
// 경로가 바로 돌게 하면 그 호출도 함께 실행된다.
const source = scripts
  .map(src => `// ===== ${src} =====\n` + fs.readFileSync('ui/' + src, 'utf8'))
  .join('\n');

// 최상위 throw만 try로 잡힌다. 첫 갱신처럼 await를 지나 터지는 오류는 거부로 오므로
// 따로 받아야 한다 — 이쪽이 실제로 창을 비게 만드는 경로다.
for (const ev of ['unhandledRejection', 'uncaughtException']) {
  process.on(ev, (e) => {
    console.error('THROW(async):', (e && e.message) || e);
    console.error(((e && e.stack) || '').split('\n').slice(0, 5).join('\n'));
    process.exit(1);
  });
}
(async () => {
  try {
    new Function(source)();
    // loading 경로: 페이지가 스크립트를 다 실행한 뒤 부르는 것과 같은 순서
    for (const fn of domReadyListeners) fn();
  } catch (e) {
    console.error('THROW:', e.constructor.name, '|', e.message);
    console.error((e.stack || '').split('\\n').slice(0, 6).join('\\n'));
    process.exit(1);
  }
  // 첫 갱신은 await를 지나 끝나므로, 큐를 비우고 나서야 통과라고 말할 수 있다
  for (let i = 0; i < 5; i++) await new Promise((r) => setImmediate(r));
  console.log(`완주 (readyState=${readyState}) — 던지지 않음`);
})();
