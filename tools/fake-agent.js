// app-smoke.py가 탭에 띄우는 가짜 에이전트. 클로드의 풀스크린 화면과 같은 조건을 만든다:
// 대체 화면·마우스 보고를 켜고, 화면을 주기적으로 다시 그리고(동기화 출력으로 묶어서),
// 키를 받으면 곧바로 몇 번째 키인지 화면 둘째 줄에 찍는다. 받자마자 답하므로 키를 누른 순간부터
// 그 숫자가 화면에 보일 때까지가 전부 앱과 ConPTY의 몫이다.
//   FAKE_FRAME_MS  다시 그리는 간격 (기본 100, 0이면 안 그림)
//   FAKE_FRAME_ROWS 한 번에 그리는 줄 수 (기본 6)
const E = "\x1b";
const frameMs = +(process.env.FAKE_FRAME_MS ?? 100);
const frameRows = +(process.env.FAKE_FRAME_ROWS ?? 6);
const out = (s) => process.stdout.write(s);

process.stdin.setRawMode(true);
process.stdin.resume();
out(`${E}[?1049h${E}[?1000h${E}[?1003h${E}[?1006h${E}[2J${E}[1;1HREADY fake-agent`);

let keys = 0;
process.stdin.on("data", (d) => {
  const s = d.toString("latin1");
  if (s === "\x03") { out(`${E}[?1049l`); process.exit(0); } // Ctrl+C
  if (s.startsWith(`${E}[<`) || s.startsWith(`${E}[I`) || s.startsWith(`${E}[O`)) return; // 마우스·포커스
  keys++;
  out(`${E}[2;1HK${keys} ${JSON.stringify(s).slice(0, 40)}${E}[K`);
});

// FAKE_MODE=rows: 클로드의 풀스크린 화면처럼 줄마다 배경색을 끝 칸까지 채워 그리고, 크기가 바뀔 때만
// 통째로 다시 그린다. 줄마다 자기 번호와 그릴 때의 칸 수가 적혀 있어, 터미널에 보이는 것이 에이전트가
// 그린 그대로인지(줄이 밀리거나 옛 줄이 남지 않았는지) 밖에서 대조할 수 있다.
let gen = 0;
function paintRows() {
  const cols = process.stdout.columns || 80, rows = process.stdout.rows || 24;
  gen++;
  let f = `${E}[?2026h`;
  for (let y = 1; y <= rows; y++) {
    const text = `R${y} c${cols}r${rows} g${gen} 한글 폭 — 확인`;
    const used = [...text].reduce((n, ch) => n + (/[가-힣]/.test(ch) ? 2 : 1), 0); // 한글은 두 칸
    f += `${E}[${y};1H${E}[48;5;${22 + (y % 2)}m${text}${" ".repeat(Math.max(0, cols - used))}`;
  }
  f += `${E}[0m${E}[?2026l`;
  out(f);
}
if (process.env.FAKE_MODE === "rows") {
  paintRows();
  process.stdout.on("resize", paintRows);
}

let k = 0;
function frame() {
  const cols = Math.max(20, (process.stdout.columns || 80) - 1);
  const rows = Math.min(frameRows, Math.max(1, (process.stdout.rows || 24) - 4));
  let f = `${E}[?2026h`;
  for (let y = 0; y < rows; y++) {
    f += `${E}[${4 + y};1H${E}[${31 + ((y + k) % 7)}m${(`frame ${k} `.repeat(40)).slice(0, cols)}`;
  }
  f += `${E}[0m${E}[?2026l`;
  k++;
  out(f);
}
if (frameMs > 0 && process.env.FAKE_MODE !== "rows") setInterval(frame, frameMs);
// 시험이 정리하지 못하고 끝나도 남지 않게
setTimeout(() => process.exit(0), 5 * 60 * 1000);
