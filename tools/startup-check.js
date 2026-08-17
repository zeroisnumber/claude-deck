// 시작 검사: main.js를 최상위부터 끝까지 실행해 도중에 던지는지 본다.
//
// 프런트가 최상위에서 한 번 던지면 창도 안 뜨고 세션 목록도 안 그려지는데,
// 컴파일도 통과하고 node --check도 통과해서 릴리스에 그대로 나간다(실제로 나갔다:
// setSidebarCollapsed가 선언 전의 profiles를 읽었다). 그 한 가지를 잡는다.
//
// DOM은 index.html에 실제로 있는 id/class만 존재하는 것으로 흉내 낸다 — 없는
// 요소를 만지는 코드도 여기서 걸린다. 실행 후 동작까지 검증하지는 않는다.
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
global.document = {
  querySelector: sel,
  querySelectorAll: () => [],
  getElementById: (id) => (ids.has(id) ? mkEl('#'+id) : null),
  createElement: () => mkEl('created'),
  addEventListener(){}, removeEventListener(){},
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

try {
  new Function(fs.readFileSync('ui/main.js', 'utf8'))();
  console.log('완주 — 최상위에서 던지지 않음');
} catch (e) {
  console.error('THROW:', e.constructor.name, '|', e.message);
  console.error((e.stack || '').split('\n').slice(0, 6).join('\n'));
  process.exit(1);
}
