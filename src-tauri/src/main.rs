// CLI Deck — 멀티 에이전트(Claude/Codex/Gemini) 세션 사이드바 + 임베디드 PTY 터미널 데스크톱 앱.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::UNIX_EPOCH,
};
use tauri::{AppHandle, Emitter, Manager, State};

// ---------- 크래시 진단 ----------
// windows_subsystem="windows"(릴리스 빌드)는 콘솔이 없어 패닉 메시지(stderr)가
// 그냥 사라진다 — "가끔 팅긴다"는 게 이거였을 가능성이 높음. 패닉 시 로그 파일에
// 기록하고 네이티브 팝업을 띄워 최소한 원인을 알 수 있게 한다.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        // WebView2도 같은 폴더(%LOCALAPPDATA%\<identifier>)에 프로필을 두므로
        // 별도 폴더를 새로 만들지 않고 거기에 합쳐서 — 삭제/관리 지점을 하나로 유지한다.
        if let Some(dir) = dirs::data_local_dir() {
            let log_dir = dir.join("com.user.cli-deck");
            if fs::create_dir_all(&log_dir).is_ok() {
                use std::io::Write as _;
                if let Ok(mut f) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_dir.join("crash.log"))
                {
                    let _ = writeln!(f, "[{}] {}", chrono_now_iso(), msg);
                }
            }
        }
        rfd::MessageDialog::new()
            .set_title("CLI Deck 오류")
            .set_description(format!(
                "예기치 않은 오류가 발생했습니다:\n\n{}\n\n로그: %LOCALAPPDATA%\\com.user.cli-deck\\crash.log",
                msg
            ))
            .set_level(rfd::MessageLevel::Error)
            .show();
        default_hook(info);
    }));
}

// ---------- PTY 트레이스 (상태 감지 판별자 실측용, 기본 꺼짐) ----------
// 배경: "출력이 있으면 작업 중"으로는 안 된다는 게 확인됐다(대기 중에도 프롬프트
// 박스 재그리기 같은 단발 출력이 있음). 그래서 판별자를 밀도 기반으로 가야 하는데,
// 임계값을 추측으로 정하지 않으려고 청크 도착 패턴을 실제로 찍어본다.
//
// 릴리스 빌드에서는 항상 꺼져 있다 (배포본에 진단 로그를 남기지 않으려고).
// 디버그 빌드에서 켜기: 환경변수 CLI_DECK_PTY_TRACE=1, 또는 데이터 폴더에 빈 파일 TRACE 생성.
//   환경변수는 그걸 설정한 셸에서 띄웠을 때만 붙어서(탐색기 더블클릭이면 안 붙는다)
//   놓치기 쉬우므로 파일 방식도 함께 지원한다 — 어떻게 실행하든 켜진다.
// 출력: %LOCALAPPDATA%\com.user.cli-deck\pty-trace.log
// 형식: <경과ms>	<pty id>	<에이전트>	<종류>	<값>
//   out    = "<바이트수>,<스피너글리프 포함 1|0>"
//   in     = 사용자 입력 바이트수 (타이핑 에코를 출력과 구분하기 위해 필요)
//   spawn  = 실행 명령, exit = 없음
//   #start = <epoch ms>	<기준 경과ms> — 벽시계 환산용 (세션 jsonl과 대조)
static PTY_TRACE: LazyLock<bool> = LazyLock::new(|| {
    // 배포되는 빌드에는 진단 계측을 남기지 않는다 — 이 분기는 컴파일 시점에
    // 상수로 접히므로 릴리스에서는 기록 경로 자체가 사라진다.
    // 다시 수집하려면 디버그 빌드(debug.bat)로 실행하면 된다.
    if !cfg!(debug_assertions) {
        return false;
    }
    let by_env = std::env::var("CLI_DECK_PTY_TRACE")
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0"
        })
        .unwrap_or(false);
    let by_file = dirs::data_local_dir()
        .map(|d| d.join("com.user.cli-deck").join("TRACE").exists())
        .unwrap_or(false);
    by_env || by_file
});

static TRACE_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
static TRACE_FILE: LazyLock<Mutex<Option<(fs::File, u64)>>> = LazyLock::new(|| Mutex::new(None));

/// 며칠 켜둬도 디스크를 잡아먹지 않도록. 샘플링 대신 상한으로 끊는다 —
/// 청크 간격 분포가 신호 자체라 솎아내면 데이터가 망가진다.
const TRACE_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// ccmanager가 Claude 감지에 쓰는 스피너 문자 집합
const SPINNER_GLYPHS: &str = "✱✲✳✴✵✶✷✸✹✺✻✼✽✾✿❀❁❂❃❇❈❉❊❋✢✣✤✥✦✧✨⊛⊕⊙◉◎◍⁂⁕※⍟☼★☆·•⏺▸▹∙⋅○●";

fn trace(id: &str, agent: &str, kind: &str, value: &str) {
    if !*PTY_TRACE {
        return;
    }
    let mut guard = TRACE_FILE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let Some(dir) = dirs::data_local_dir() else { return };
        let dir = dir.join("com.user.cli-deck");
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        let Ok(f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("pty-trace.log"))
        else {
            return;
        };
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        *guard = Some((f, size));
        // 경과 ms만으로는 세션 jsonl의 타임스탬프와 대조할 수 없다 —
        // "이 구간이 권한 대기였나 작업중이었나"의 정답이 거기 있으므로
        // 실행마다 기준점을 남겨 벽시계 시각으로 환산할 수 있게 한다.
        // wall_ms = epoch_ms + (경과ms - 기준경과ms)
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let header = format!("#start\t{}\t{}\n", epoch_ms, TRACE_START.elapsed().as_millis());
        if let Some((f, written)) = guard.as_mut() {
            use std::io::Write as _;
            if f.write_all(header.as_bytes()).is_ok() {
                *written += header.len() as u64;
            }
        }
    }
    let Some((f, written)) = guard.as_mut() else { return };
    if *written >= TRACE_MAX_BYTES {
        return;
    }
    let line = format!(
        "{}\t{}\t{}\t{}\t{}\n",
        TRACE_START.elapsed().as_millis(),
        id,
        agent,
        kind,
        value
    );
    use std::io::Write as _;
    if f.write_all(line.as_bytes()).is_ok() {
        *written += line.len() as u64;
    }
}

/// 세 에이전트의 출력 분포가 섞이면 아무것도 못 배우므로 실행 명령에서 라벨을 뽑는다
fn trace_agent_label(command: &str) -> String {
    let c = command.to_lowercase();
    for name in ["codex", "gemini", "claude"] {
        if c.contains(name) {
            return name.to_string();
        }
    }
    "other".to_string()
}

/// 멀티바이트 글리프가 청크 경계에서 잘릴 수 있어 직전 청크의 꼬리 몇 바이트를 이어 붙여
/// 검사한다. 경계에 걸친 글리프가 다음 청크로 밀려 잡힐 수는 있으나, 기록하는 건
/// 청크당 불리언 하나라 분석에는 영향이 없다.
fn has_spinner_glyph(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).chars().any(|c| SPINNER_GLYPHS.contains(c))
}

// ---------- 작업 상태 판정 ----------
// 전부 pty-trace.log 실측으로 정한 값이다. 근거는 각 상수에 적어둔다.
// 핵심 구조: "지금 작업 중인가"는 PTY 출력 밀도가 답하고(Rust에서 판정하므로
// WebView 백그라운드 스로틀링과 무관), "턴이 끝났는가"는 세션 파일이 답한다.
// 침묵 길이로 완료를 판정할 수 없다는 게 실측으로 확인됐다 — 턴 내부 침묵이
// 78.7초까지 관측된 반면 턴 종료 침묵이 28.7초인 사례가 있어 분포가 역전된다.

/// 입력 후 이 시간 안에 온 출력은 타이핑 에코로 보고 활동에서 제외.
/// 실측: 300~1200ms 중 어느 값을 써도 결과가 같았다(민감하지 않음).
const ECHO_MS: u64 = 800;
/// 이보다 벌어지면 다른 버스트. 실측: 작업 중 청크 간격 p90 = 145ms.
const BURST_GAP_MS: u64 = 500;
/// 버스트가 이보다 길면 작업 중. 실측: 대기 중 단발 출력은 최대 250ms,
/// 가장 짧은 에이전트 버스트는 1.15초로 사이가 비어 있다.
const BURST_MIN_MS: u64 = 1000;
/// 파일이 "아직 작업 중"이라고 말해도 이만큼 조용하면 강제로 완료 처리.
/// 세션 파일이 없는 탭(새 세션·Gemini)의 유일한 완료 판정이기도 하다.
/// 실측된 턴 내부 최장 침묵 78.7초에 여유를 둔 값.
const MAX_WORKING_SILENCE_MS: u64 = 120_000;
/// 버스트가 끝난 뒤 파일을 다시 확인하는 간격 (250ms마다 읽지 않도록)
const FILE_RECHECK_MS: u64 = 2_000;

struct Activity {
    last_input: Option<std::time::Instant>,
    last_out: Option<std::time::Instant>,
    burst_start: Option<std::time::Instant>,
    working: bool,
    last_check: Option<std::time::Instant>,
    agent: String,
    /// 세션 파일 — 턴 종료 판정용. 없으면(새 세션) 침묵 타임아웃만 쓴다.
    file: Option<PathBuf>,
}

static ACTIVITY: LazyLock<Mutex<HashMap<String, Activity>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Serialize)]
struct PtyStateEvent {
    id: String,
    working: bool,
}

/// 파일 끝부분만 읽는다 (턴 상태는 마지막 레코드에만 있음)
fn read_tail(path: &std::path::Path, limit: u64) -> Option<String> {
    use std::io::{Read as _, Seek, SeekFrom};
    let size = fs::metadata(path).ok()?.len();
    let mut f = fs::File::open(path).ok()?;
    if size > limit {
        f.seek(SeekFrom::End(-(limit as i64))).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 턴이 아직 진행 중인가. "작업 중"을 화이트리스트로 정의한다 —
/// 오류·한도 초과·중단은 종류를 열거할 수 없고(실측에서 stop_sequence로 끝난
/// 한도 초과 턴이 나왔다), 열거를 놓치면 탭이 영영 작업중으로 남는다.
fn turn_in_progress(file: &std::path::Path) -> bool {
    // Gemini는 턴마다 append하지 않고 통짜 JSON을 다시 쓰므로 신호가 없다.
    // 이런 탭은 침묵 타임아웃에만 의존한다.
    if file.extension().map(|e| e != "jsonl").unwrap_or(true) {
        return false;
    }
    let Some(text) = read_tail(file, 32 * 1024) else { return false };
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if o["isSidechain"] == true {
            continue; // 서브에이전트 레코드는 메인 턴 상태가 아니다
        }
        match o["type"].as_str().unwrap_or("") {
            "assistant" => {
                // 툴 호출로 끝났으면 결과를 기다리는 중 = 진행 중.
                // end_turn·stop_sequence·그 밖의 무엇이든 턴은 끝난 것으로 본다.
                return o["message"]["stop_reason"] == "tool_use";
            }
            "user" => {
                let txt = extract_text(&o["message"]["content"]);
                if txt.trim().starts_with("[Request interrupted") {
                    return false; // 사용자가 중단함
                }
                // 프롬프트 제출 또는 tool_result → 에이전트 차례
                return true;
            }
            // codex: 마지막 이벤트가 에이전트 응답이면 끝난 것으로 본다
            "event_msg" => match o["payload"]["type"].as_str().unwrap_or("") {
                "agent_message" => return false,
                "user_message" => return true,
                _ => continue,
            },
            _ => continue,
        }
    }
    false
}

/// 출력 청크 도착 — 타이핑 에코가 아니면 버스트를 잇는다
fn note_output(id: &str) {
    let now = std::time::Instant::now();
    let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(a) = act.get_mut(id) else { return };
    let echo = a
        .last_input
        .map(|t| now.duration_since(t).as_millis() as u64 <= ECHO_MS)
        .unwrap_or(false);
    if echo {
        return;
    }
    let gap = a
        .last_out
        .map(|t| now.duration_since(t).as_millis() as u64)
        .unwrap_or(u64::MAX);
    if gap >= BURST_GAP_MS {
        a.burst_start = Some(now);
    }
    a.last_out = Some(now);
}

/// 버스트 상태를 주기적으로 평가해 working 전이를 이벤트로 올린다.
/// JS 타이머가 아니라 여기서 판정하는 게 요점 — 창이 백그라운드로 가도 멈추지 않는다.
fn spawn_state_monitor(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let now = std::time::Instant::now();
        let ms = |a: std::time::Instant, b: std::time::Instant| b.duration_since(a).as_millis() as u64;

        // 1단계: 잠금 안에서 판정에 필요한 것만 모은다 (파일 I/O는 잠금 밖에서)
        let mut turn_on: Vec<String> = Vec::new();
        let mut candidates: Vec<(String, Option<PathBuf>, u64)> = Vec::new();
        {
            let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
            for (id, a) in act.iter_mut() {
                let (Some(bs), Some(lo)) = (a.burst_start, a.last_out) else { continue };
                let silence = ms(lo, now);
                if silence < BURST_GAP_MS {
                    if !a.working && ms(bs, lo) >= BURST_MIN_MS {
                        a.working = true;
                        trace(id, &a.agent, "state", "working");
                        turn_on.push(id.clone());
                    }
                } else if a.working {
                    let due = a.last_check.map(|t| ms(t, now) >= FILE_RECHECK_MS).unwrap_or(true);
                    if due {
                        a.last_check = Some(now);
                        candidates.push((id.clone(), a.file.clone(), silence));
                    }
                }
            }
        }

        // 2단계: 잠금 밖에서 세션 파일 확인
        let mut turn_off: Vec<String> = Vec::new();
        for (id, file, silence) in candidates {
            let done = silence >= MAX_WORKING_SILENCE_MS
                || match &file {
                    Some(p) => !turn_in_progress(p),
                    None => false, // 파일이 없으면 타임아웃까지 기다린다
                };
            if done {
                turn_off.push(id);
            }
        }

        // 3단계: 확정된 것만 반영
        if !turn_off.is_empty() {
            let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
            turn_off.retain(|id| match act.get_mut(id) {
                Some(a) if a.working => {
                    a.working = false;
                    trace(id, &a.agent, "state", "idle");
                    true
                }
                _ => false,
            });
        }
        for id in turn_on {
            let _ = app.emit("pty-state", PtyStateEvent { id, working: true });
        }
        for id in turn_off {
            let _ = app.emit("pty-state", PtyStateEvent { id, working: false });
        }
    });
}

// ---------- PTY 관리 ----------

struct PtyInstance {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// 트레이스 라벨 (claude/codex/gemini) — write_pty에서 입력 이벤트를 찍을 때 씀
    agent: String,
    /// 같은 id로 재시작(kill 직후 spawn)했을 때, 죽은 이전 프로세스의 대기 스레드가
    /// 새 인스턴스를 지워버리지 않도록 구분하는 세대 번호
    generation: u64,
}

static PTY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 프로세스 종료 처리 — 이 세대가 아직 유효할 때만 맵에서 지우고 이벤트를 보낸다.
/// (kill_pty로 이미 정리됐거나 재시작된 경우에는 아무것도 하지 않음)
fn finish_pty(app: &AppHandle, id: &str, generation: u64) {
    let state = app.state::<PtyState>();
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    match map.get(id) {
        Some(p) if p.generation == generation => {}
        _ => return,
    }
    map.remove(id);
    drop(map);
    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    let _ = app.emit("pty-exit", PtyExit { id: id.to_string() });
}

#[derive(Default)]
struct PtyState(Mutex<HashMap<String, PtyInstance>>);

#[derive(Clone, Serialize)]
struct PtyOutput {
    id: String,
    data: String, // base64
}

#[derive(Clone, Serialize)]
struct PtyExit {
    id: String,
}

#[tauri::command]
fn spawn_pty(
    app: AppHandle,
    state: State<PtyState>,
    id: String,
    cwd: String,
    command: String,
    // 세션 파일 경로 — 턴 종료 판정용. 새 세션은 아직 파일이 없어 None.
    file: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if map.contains_key(&id) {
        return Ok(()); // 이미 실행 중
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    // 최종 명령은 프런트에서 합성됨: [래퍼 접두사] + 에이전트 명령 + [--resume <세션ID>]
    let claude_cmd = if command.trim().is_empty() { "claude".to_string() } else { command };
    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.args(["/c", &claude_cmd]);
    let workdir = if PathBuf::from(&cwd).is_dir() {
        cwd.clone()
    } else {
        dirs::home_dir().unwrap_or_default().to_string_lossy().to_string()
    };
    cmd.cwd(&workdir);

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let killer = child.clone_killer();
    let generation = PTY_GENERATION.fetch_add(1, Ordering::Relaxed);

    let agent = trace_agent_label(&claude_cmd);
    trace(&id, &agent, "spawn", &claude_cmd);

    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).insert(
        id.clone(),
        Activity {
            last_input: None,
            last_out: None,
            burst_start: None,
            working: false,
            last_check: None,
            agent: agent.clone(),
            file: file.filter(|f| !f.trim().is_empty()).map(PathBuf::from),
        },
    );

    // 출력 스트리밍 스레드
    let app2 = app.clone();
    let id2 = id.clone();
    let agent2 = agent.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut carry: Vec<u8> = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if *PTY_TRACE {
                        let mut scan = std::mem::take(&mut carry);
                        scan.extend_from_slice(&buf[..n]);
                        let spin = u8::from(has_spinner_glyph(&scan));
                        carry = buf[n.saturating_sub(3)..n].to_vec();
                        trace(&id2, &agent2, "out", &format!("{},{}", n, spin));
                    }
                    note_output(&id2);
                    let data = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                    let _ = app2.emit("pty-output", PtyOutput { id: id2.clone(), data });
                }
            }
        }
        trace(&id2, &agent2, "exit", "");
        finish_pty(&app2, &id2, generation);
    });

    // 종료 감지 스레드 — ConPTY는 자식이 죽어도 마스터 쪽 read가 EOF를 돌려주지
    // 않는다(테스트 child_wait_returns_when_process_exits 참고). 리더 EOF만 믿으면
    // 탭이 영원히 "실행 중"으로 남으므로 자식을 직접 기다린다.
    // 단, 자식이 죽는 순간에도 리더 스레드에는 아직 흘려보내지 못한 출력이 남아 있을 수
    // 있다. 곧바로 pty-exit을 쏘면 "── 프로세스가 종료되었습니다 ──"가 마지막 출력보다
    // 먼저 찍히므로 잠깐 배출 시간을 준다.
    let app3 = app.clone();
    let id3 = id.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(250));
        finish_pty(&app3, &id3, generation);
    });

    map.insert(id, PtyInstance { master: pair.master, writer, killer, agent, generation });
    Ok(())
}

#[tauri::command]
fn write_pty(state: State<PtyState>, id: String, data: String) -> Result<(), String> {
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get_mut(&id) {
        trace(&id, &p.agent, "in", &data.len().to_string());
        if let Some(a) = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
            a.last_input = Some(std::time::Instant::now());
        }
        p.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn resize_pty(state: State<PtyState>, id: String, cols: u16, rows: u16) -> Result<(), String> {
    let map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get(&id) {
        p.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn kill_pty(state: State<PtyState>, id: String) -> Result<(), String> {
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut p) = map.remove(&id) {
        let _ = p.killer.kill();
    }
    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    Ok(())
}

// ---------- 세션 스캔 ----------

#[derive(Serialize, Clone)]
struct SessionMeta {
    session_id: String,
    agent: String, // "claude" | "codex" | "gemini"
    cwd: String,
    summary: Option<String>,
    first_prompt: Option<String>,
    /// recent와 마찬가지로 미리보기 전용 — 목록 응답에서는 제외한다
    #[serde(skip_serializing)]
    last_text: Option<String>,
    message_count: u32,
    mtime: f64,
    file: String,
    /// 마지막으로 프롬프트 캐시가 읽히거나 새로 쓰인 시각 (epoch seconds)
    cache_last_ts: Option<f64>,
    /// 해당 캐시 항목의 TTL (초) — 5분(300) 또는 1시간(3600)
    cache_ttl_secs: Option<u32>,
    /// 마지막 assistant 응답 시점의 컨텍스트 토큰 수 (사이드바 게이지용)
    ctx_tokens: Option<u64>,
    /// codex는 파일에 컨텍스트 윈도우가 직접 기록됨 (claude는 프런트에서 모델명으로 추정)
    ctx_window: Option<u64>,
    /// 마지막으로 관측된 모델명
    model: Option<String>,
    /// 호버 미리보기용 최근 대화 (최대 3턴 = 6개). 오래된 것부터 순서대로.
    /// 목록에는 싣지 않는다 — 세션 수 × 3KB가 20초마다 IPC로 넘어가는데
    /// 프런트는 호버할 때만 쓰므로 session_preview로 그때 가져간다.
    #[serde(skip_serializing)]
    recent: Vec<RecentMsg>,
}

/// 호버 미리보기 전용 페이로드 (목록에서 제외한 무거운 필드만)
#[derive(Serialize)]
struct SessionPreview {
    last_text: Option<String>,
    recent: Vec<RecentMsg>,
}

/// 사이드바 목록용 경량 사본 — 무거운 필드는 복사조차 하지 않는다.
fn light_meta(m: &SessionMeta) -> SessionMeta {
    SessionMeta {
        session_id: m.session_id.clone(),
        agent: m.agent.clone(),
        cwd: m.cwd.clone(),
        summary: m.summary.clone(),
        first_prompt: m.first_prompt.clone(),
        last_text: None,
        message_count: m.message_count,
        mtime: m.mtime,
        file: m.file.clone(),
        cache_last_ts: m.cache_last_ts,
        cache_ttl_secs: m.cache_ttl_secs,
        ctx_tokens: m.ctx_tokens,
        ctx_window: m.ctx_window,
        model: m.model.clone(),
        recent: Vec::new(),
    }
}

/// 파일 경로로 파서를 고른다 (claude/codex/gemini 저장소 구조가 서로 다름)
fn parser_for(path: &std::path::Path) -> fn(&PathBuf) -> Option<SessionMeta> {
    let s = path.to_string_lossy();
    if s.contains(".codex") {
        read_codex_meta
    } else if s.contains(".gemini") {
        read_gemini_meta
    } else {
        read_meta
    }
}

/// 호버 시점에만 호출 — 대개 목록 스캔이 이미 채워둔 캐시에서 바로 나온다.
#[tauri::command]
fn session_preview(file: String) -> Option<SessionPreview> {
    let p = PathBuf::from(&file);
    let m = cached_meta(&p, parser_for(&p))?;
    Some(SessionPreview { last_text: m.last_text, recent: m.recent })
}

#[derive(Serialize, Clone)]
struct RecentMsg {
    role: String, // "user" | "assistant"
    text: String,
}

const RECENT_MAX: usize = 6;

fn push_recent(recent: &mut Vec<RecentMsg>, role: &str, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    recent.push(RecentMsg {
        role: role.to_string(),
        text: text.chars().take(400).collect(),
    });
    if recent.len() > RECENT_MAX {
        recent.remove(0);
    }
}

fn file_mtime(path: &std::path::Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 큰 파일은 head+tail만 읽는다 (codex rollout은 시스템 프롬프트 포함으로 수 MB 가능)
fn read_head_tail(path: &std::path::Path, limit: u64) -> Option<String> {
    use std::io::{Read as _, Seek, SeekFrom};
    let size = fs::metadata(path).ok()?.len();
    if size <= limit {
        return fs::read_to_string(path).ok();
    }
    let mut f = fs::File::open(path).ok()?;
    let half = limit / 2;
    let mut head = vec![0u8; half as usize];
    f.read_exact(&mut head).ok()?;
    f.seek(SeekFrom::End(-(half as i64))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    Some(format!(
        "{}\n{}",
        String::from_utf8_lossy(&head),
        String::from_utf8_lossy(&tail)
    ))
}

/// 세션 메타 캐시 — 20초 폴링마다 전체 jsonl을 재파싱하지 않도록 mtime이 같으면 재사용.
/// 파싱 실패(None)도 캐시해 손상 파일을 매번 다시 읽지 않는다.
static META_CACHE: LazyLock<Mutex<HashMap<String, (f64, Option<SessionMeta>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cached_meta_with(
    path: &PathBuf,
    parse: fn(&PathBuf) -> Option<SessionMeta>,
    pick: fn(&SessionMeta) -> SessionMeta,
) -> Option<SessionMeta> {
    let mtime = file_mtime(path);
    let key = path.to_string_lossy().to_string();
    if let Some((t, m)) = META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if *t == mtime {
            return m.as_ref().map(pick);
        }
    }
    let meta = parse(path);
    let picked = meta.as_ref().map(pick);
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (mtime, meta));
    picked
}

fn cached_meta(path: &PathBuf, parse: fn(&PathBuf) -> Option<SessionMeta>) -> Option<SessionMeta> {
    cached_meta_with(path, parse, |m| m.clone())
}

/// 목록 스캔용 — 캐시에서 경량 필드만 복사한다
fn cached_meta_light(path: &PathBuf, parse: fn(&PathBuf) -> Option<SessionMeta>) -> Option<SessionMeta> {
    cached_meta_with(path, parse, light_meta)
}

/// 사라진 세션 파일의 캐시 항목 정리 (없으면 무한히 쌓임)
fn evict_stale_cache() {
    META_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|k, _| std::path::Path::new(k).exists());
}

/// "YYYY-MM-DDTHH:MM:SS.sssZ" (Claude jsonl의 고정 포맷) → epoch seconds.
/// 외부 크레이트 없이 Howard Hinnant의 civil_from_days 역산 공식을 사용.
fn parse_iso_ts(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let se: i64 = s.get(17..19)?.parse().ok()?;
    let ms: f64 = s.get(20..23).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);

    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    Some((days * 86400 + h * 3600 + mi * 60 + se) as f64 + ms / 1000.0)
}

fn extract_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn read_meta(path: &PathBuf) -> Option<SessionMeta> {
    let mtime = file_mtime(path);
    // 큰 세션 파일(장기 세션)은 codex와 동일하게 head+tail만 읽어 폴링 부하를 낮춘다.
    // first_prompt는 head, last_text/캐시 TTL/summary는 tail에서 나오므로 손실 없음
    // (중간 구간의 message_count만 근사치가 됨).
    let text = read_head_tail(path, 512 * 1024)?;

    let mut meta = SessionMeta {
        session_id: path.file_stem()?.to_string_lossy().to_string(),
        agent: "claude".into(),
        cwd: String::new(),
        summary: None,
        first_prompt: None,
        last_text: None,
        message_count: 0,
        mtime,
        file: path.to_string_lossy().to_string(),
        cache_last_ts: None,
        cache_ttl_secs: None,
        ctx_tokens: None,
        ctx_window: None,
        model: None,
        recent: Vec::new(),
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if meta.cwd.is_empty() {
            if let Some(c) = obj["cwd"].as_str() {
                meta.cwd = c.to_string();
            }
        }
        if obj["type"] == "summary" {
            if let Some(s) = obj["summary"].as_str() {
                meta.summary = Some(s.to_string());
            }
        }
        let t = obj["type"].as_str().unwrap_or("");
        if t == "user" || t == "assistant" {
            meta.message_count += 1;
            if t == "user" && obj["isMeta"] != true {
                let txt = extract_text(&obj["message"]["content"]);
                let txt = txt.trim();
                // "[Request interrupted by user]"는 사용자가 친 프롬프트가 아니라
                // 중단 마커라서 제목/미리보기에 뜨면 안 된다
                if !txt.is_empty()
                    && !txt.starts_with('<')
                    && !txt.starts_with("Caveat:")
                    && !txt.starts_with("[Request interrupted")
                {
                    if meta.first_prompt.is_none() {
                        meta.first_prompt = Some(txt.chars().take(120).collect());
                    }
                    push_recent(&mut meta.recent, "user", txt);
                }
            }
            if t == "assistant" {
                let txt = extract_text(&obj["message"]["content"]);
                let txt = txt.trim();
                if !txt.is_empty() {
                    meta.last_text = Some(txt.chars().take(1200).collect());
                    push_recent(&mut meta.recent, "assistant", txt);
                }

                // 프롬프트 캐시 TTL 추적: 이 레코드가 캐시를 읽었거나 새로 썼으면
                // 해당 시각부터 TTL이 (재)시작된 것으로 본다. 5분/1시간 중 실제
                // 쓰기가 발생한 티어를 우선하고, 읽기만 있었다면 이전에 관찰된
                // 티어를 유지한다(Anthropic 캐시는 5분 기본, 세션 내 1시간 명시 가능).
                let u = &obj["message"]["usage"];
                let read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                let w1h = u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
                let w5m = u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0);
                if read > 0 || w1h > 0 || w5m > 0 {
                    if let Some(ts) = obj["timestamp"].as_str().and_then(parse_iso_ts) {
                        meta.cache_last_ts = Some(ts);
                        if w1h > 0 {
                            meta.cache_ttl_secs = Some(3600);
                        } else if meta.cache_ttl_secs.is_none() {
                            meta.cache_ttl_secs = Some(300); // 5분 쓰기 또는 티어 미관찰(읽기만) 시 기본값
                        }
                    }
                }

                // 사이드바 컨텍스트 게이지용: 마지막 assistant 응답의 컨텍스트 크기
                // (이미 읽어둔 tail을 재사용하므로 추가 I/O 없음)
                let ctx = u["input_tokens"].as_u64().unwrap_or(0) + read
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                if ctx > 0 {
                    meta.ctx_tokens = Some(ctx);
                    meta.model = obj["message"]["model"].as_str().map(|s| s.to_string());
                }
            }
        }
    }
    Some(meta)
}

// ---------- Codex 세션 (~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl) ----------

fn read_codex_meta(path: &PathBuf) -> Option<SessionMeta> {
    let text = read_head_tail(path, 512 * 1024)?;
    let mut meta = SessionMeta {
        session_id: String::new(),
        agent: "codex".into(),
        cwd: String::new(),
        summary: None,
        first_prompt: None,
        last_text: None,
        message_count: 0,
        mtime: file_mtime(path),
        file: path.to_string_lossy().to_string(),
        cache_last_ts: None,
        cache_ttl_secs: None,
        ctx_tokens: None,
        ctx_window: None,
        model: None,
        recent: Vec::new(),
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match obj["type"].as_str().unwrap_or("") {
            "session_meta" => {
                if let Some(id) = obj["payload"]["id"].as_str() {
                    meta.session_id = id.to_string();
                }
                if let Some(c) = obj["payload"]["cwd"].as_str() {
                    meta.cwd = c.to_string();
                }
            }
            "event_msg" => match obj["payload"]["type"].as_str().unwrap_or("") {
                "user_message" => {
                    meta.message_count += 1;
                    if let Some(m) = obj["payload"]["message"].as_str() {
                        let m = m.trim();
                        if !m.is_empty() {
                            if meta.first_prompt.is_none() {
                                meta.first_prompt = Some(m.chars().take(120).collect());
                            }
                            push_recent(&mut meta.recent, "user", m);
                        }
                    }
                }
                "agent_message" => {
                    meta.message_count += 1;
                    if let Some(m) = obj["payload"]["message"].as_str() {
                        let m = m.trim();
                        if !m.is_empty() {
                            meta.last_text = Some(m.chars().take(1200).collect());
                            push_recent(&mut meta.recent, "assistant", m);
                        }
                    }
                }
                "token_count" => {
                    let info = &obj["payload"]["info"];
                    if let Some(tot) = info["total_token_usage"]["total_tokens"].as_u64() {
                        meta.ctx_tokens = Some(tot);
                    }
                    if let Some(w) = info["model_context_window"].as_u64() {
                        meta.ctx_window = Some(w);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if meta.session_id.is_empty() {
        return None;
    }
    Some(meta)
}

fn scan_codex(out: &mut Vec<SessionMeta>) {
    let Some(home) = dirs::home_dir() else { return };
    let root = home.join(".codex").join("sessions");
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                if let Some(m) = cached_meta_light(&p, read_codex_meta) {
                    out.push(m);
                }
            }
        }
    }
}

// ---------- Gemini 세션 (~/.gemini/tmp/<proj>/chats/session-*.json) ----------

fn gemini_project_paths(home: &std::path::Path) -> std::collections::HashMap<String, String> {
    // projects.json: { "projects": { "c:\\workspace\\foo": "foo", ... } } — 폴더명 → 실제 경로 역매핑
    let mut map = std::collections::HashMap::new();
    let Ok(text) = fs::read_to_string(home.join(".gemini").join("projects.json")) else { return map };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return map };
    if let Some(obj) = v["projects"].as_object() {
        for (path, name) in obj {
            if let Some(n) = name.as_str() {
                map.insert(n.to_string(), path.clone());
            }
        }
    }
    map
}

/// gemini json 파싱 — cwd에는 프로젝트 폴더명(원시)을 임시로 넣어 두고,
/// 호출부에서 projects.json 매핑을 거쳐 실제 경로로 치환한다
/// (cached_meta가 요구하는 fn(&PathBuf) -> Option<SessionMeta> 시그니처는 캡처를 허용하지 않음).
fn read_gemini_meta(path: &PathBuf) -> Option<SessionMeta> {
    let text = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let sid = v["sessionId"].as_str()?;
    let msgs = v["messages"].as_array().cloned().unwrap_or_default();
    let first = msgs.iter().find(|m| m["type"] == "user").and_then(|m| {
        m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str()))
    });
    let last = msgs.iter().rev().find(|m| m["type"] != "user").and_then(|m| {
        m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str()))
    });
    let name = path.parent()?.parent()?.file_name()?.to_string_lossy().to_string();

    let mut recent = Vec::new();
    for m in msgs.iter().rev().take(RECENT_MAX) {
        let role = if m["type"] == "user" { "user" } else { "assistant" };
        let txt = m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str())).unwrap_or("");
        push_recent(&mut recent, role, txt);
    }
    recent.reverse();

    Some(SessionMeta {
        session_id: sid.to_string(),
        agent: "gemini".into(),
        cwd: name,
        summary: None,
        first_prompt: first.map(|s| s.trim().chars().take(120).collect()),
        last_text: last.map(|s| s.trim().chars().take(1200).collect()),
        message_count: msgs.len() as u32,
        mtime: file_mtime(path),
        file: path.to_string_lossy().to_string(),
        cache_last_ts: None,
        cache_ttl_secs: None,
        ctx_tokens: None,
        ctx_window: None,
        model: None,
        recent,
    })
}

fn scan_gemini(out: &mut Vec<SessionMeta>) {
    let Some(home) = dirs::home_dir() else { return };
    let proj_map = gemini_project_paths(&home);
    let root = home.join(".gemini").join("tmp");
    let Ok(projects) = fs::read_dir(&root) else { return };
    for proj in projects.flatten() {
        let chats = proj.path().join("chats");
        let Ok(files) = fs::read_dir(&chats) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                if let Some(mut meta) = cached_meta_light(&p, read_gemini_meta) {
                    if let Some(real) = proj_map.get(&meta.cwd) {
                        meta.cwd = real.clone();
                    }
                    out.push(meta);
                }
            }
        }
    }
}

#[tauri::command]
fn list_sessions() -> Vec<SessionMeta> {
    evict_stale_cache();
    let mut out = Vec::new();
    scan_codex(&mut out);
    scan_gemini(&mut out);
    let Some(home) = dirs::home_dir() else { return out };
    let projects = home.join(".claude").join("projects");
    let Ok(dirs_iter) = fs::read_dir(&projects) else { return out };

    for proj in dirs_iter.flatten() {
        let Ok(files) = fs::read_dir(proj.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Some(mut meta) = cached_meta_light(&path, read_meta) {
                    if meta.cwd.is_empty() {
                        // 폴더명(C--workspace-foo)에서 경로 근사 복원 (비ASCII 폴더명 바이트 경계 패닉 방지)
                        let name = proj.file_name().to_string_lossy().to_string();
                        meta.cwd = match (name.get(0..1), name.get(1..3), name.get(3..)) {
                            (Some(d), Some("--"), Some(rest)) if !rest.is_empty() => {
                                format!("{}:\\{}", d, rest.replace('-', "\\"))
                            }
                            _ => name,
                        };
                    }
                    out.push(meta);
                }
            }
        }
    }
    out.sort_by(|a, b| b.mtime.partial_cmp(&a.mtime).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// ---------- 사용량 통계 (세션 jsonl의 usage 레코드 기반 — 프록시 불필요) ----------

// 열린 탭의 컨텍스트 게이지는 list_sessions가 이미 내려주는 ctx_tokens/ctx_window로
// 프런트에서 계산한다 (예전 session_usage 커맨드는 같은 계산을 위해 탭마다 8초 주기로
// 세션 파일을 다시 읽고 있었다).

#[derive(Serialize, Default, Clone)]
struct UsageRow {
    date: String,
    model: String,
    cwd: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_5m: u64,
    cache_1h: u64,
    requests: u64,
}

type UsageKey = (String, String, String); // (날짜, 모델, 프로젝트)

/// 세션 파일 하나를 (날짜, 모델, 프로젝트)별로 집계
fn usage_rows_of_file(path: &PathBuf) -> Vec<UsageRow> {
    let Ok(text) = fs::read_to_string(path) else { return vec![] };
    let mut map: HashMap<UsageKey, UsageRow> = HashMap::new();
    let mut cwd = String::new();
    for line in text.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        if cwd.is_empty() {
            if let Some(c) = obj["cwd"].as_str() {
                cwd = c.to_string();
            }
        }
        if obj["type"] != "assistant" {
            continue;
        }
        let u = &obj["message"]["usage"];
        if u.is_null() {
            continue;
        }
        let ts = obj["timestamp"].as_str().unwrap_or("");
        if ts.len() < 10 {
            continue;
        }
        let date = ts[..10].to_string();
        let model = obj["message"]["model"].as_str().unwrap_or("?").to_string();
        let row = map
            .entry((date.clone(), model.clone(), cwd.clone()))
            .or_insert_with(|| UsageRow { date, model, cwd: cwd.clone(), ..Default::default() });
        row.input += u["input_tokens"].as_u64().unwrap_or(0);
        row.output += u["output_tokens"].as_u64().unwrap_or(0);
        row.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        row.cache_5m += u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0);
        row.cache_1h += u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
        row.requests += 1;
    }
    map.into_values().collect()
}

/// 파일별 집계 캐시 — 대시보드를 열 때마다 최근 N일치 jsonl을 전량 다시 읽지 않도록
/// mtime이 그대로면 재사용한다 (세션 목록의 META_CACHE와 같은 전략).
static USAGE_FILE_CACHE: LazyLock<Mutex<HashMap<String, (f64, Vec<UsageRow>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 대시보드용: 최근 N일간 (날짜, 모델, 프로젝트)별 토큰 집계
#[tauri::command]
fn usage_stats(days: u32) -> Vec<UsageRow> {
    let mut map: HashMap<UsageKey, UsageRow> = HashMap::new();
    let Some(home) = dirs::home_dir() else { return vec![] };
    let projects = home.join(".claude").join("projects");
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(days as u64 * 86400 + 86400);
    let Ok(dirs_iter) = fs::read_dir(&projects) else { return vec![] };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for proj in dirs_iter.flatten() {
        let Ok(files) = fs::read_dir(proj.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if !p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                continue;
            }
            // 추가 기록은 mtime을 갱신하므로 오래된 파일은 통째로 건너뜀
            if fs::metadata(&p)
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(true)
            {
                continue;
            }
            let key = p.to_string_lossy().to_string();
            let mtime = file_mtime(&p);
            seen.insert(key.clone());
            let cached = {
                let cache = USAGE_FILE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
                match cache.get(&key) {
                    Some((t, rows)) if *t == mtime => Some(rows.clone()),
                    _ => None,
                }
            };
            let rows = cached.unwrap_or_else(|| {
                let rows = usage_rows_of_file(&p);
                USAGE_FILE_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, (mtime, rows.clone()));
                rows
            });
            for r in rows {
                let entry = map
                    .entry((r.date.clone(), r.model.clone(), r.cwd.clone()))
                    .or_insert_with(|| UsageRow {
                        date: r.date.clone(),
                        model: r.model.clone(),
                        cwd: r.cwd.clone(),
                        ..Default::default()
                    });
                entry.input += r.input;
                entry.output += r.output;
                entry.cache_read += r.cache_read;
                entry.cache_5m += r.cache_5m;
                entry.cache_1h += r.cache_1h;
                entry.requests += r.requests;
            }
        }
    }
    // 기간 밖으로 밀려났거나 삭제된 파일의 캐시는 버린다
    USAGE_FILE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|k, _| seen.contains(k));
    map.into_values().collect()
}

// ---------- 요금제 한도 (5시간/주간 사용률 + 리셋 시각) ----------
// 기본: Claude Code OAuth 토큰으로 사용량 API 직접 조회 (headroom 불필요)
// 폴백: headroom이 폴링해둔 subscription_state.json

fn oauth_token() -> Option<String> {
    if let Ok(t) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"));
    let creds: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(base.join(".credentials.json")).ok()?).ok()?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"].as_str()?.to_string();
    // 만료 확인 (ms 단위)
    if let Some(exp) = oauth["expiresAt"].as_f64() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as f64;
        if now_ms >= exp - 60_000.0 {
            return None;
        }
    }
    Some(token)
}

fn fetch_usage_direct() -> Option<serde_json::Value> {
    let token = oauth_token()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().ok()?;
    let map_win = |w: &serde_json::Value| {
        serde_json::json!({
            "utilization_pct": w["utilization"],
            "resets_at": w["resets_at"],
        })
    };
    Some(serde_json::json!({
        "source": "direct",
        "five_hour": map_win(&v["five_hour"]),
        "seven_day": map_win(&v["seven_day"]),
        "limits": v["limits"],
        "polled_at": chrono_now_iso(),
    }))
}

fn chrono_now_iso() -> String {
    // 의존성 없이 대략적인 ISO 시각 (frontend는 상대시간 계산에 resets_at만 사용)
    let secs = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{}", secs)
}

fn usage_from_headroom() -> Option<serde_json::Value> {
    let p = dirs::home_dir()?.join(".headroom").join("subscription_state.json");
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(p).ok()?).ok()?;
    if v["latest"].is_null() {
        return None;
    }
    let mut latest = v["latest"].clone();
    latest["source"] = serde_json::json!("headroom");
    Some(latest)
}

// ---------- Codex 상태 (rollout 파일의 token_count 이벤트에서 로컬로 추출) ----------

fn codex_rollouts_by_mtime() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return vec![] };
    let mut files: Vec<(f64, PathBuf)> = Vec::new();
    let mut stack = vec![home.join(".codex").join("sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                files.push((file_mtime(&p), p));
            }
        }
    }
    files.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    files.into_iter().map(|(_, p)| p).collect()
}

/// 가장 최근 codex 세션의 마지막 token_count 이벤트에서 rate limit 추출
#[tauri::command]
fn codex_state() -> Option<serde_json::Value> {
    for p in codex_rollouts_by_mtime().into_iter().take(3) {
        let Some(text) = read_head_tail(&p, 256 * 1024) else { continue };
        let mut last: Option<(String, serde_json::Value)> = None;
        for line in text.lines() {
            let Ok(o) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
            if o["type"] == "event_msg" && o["payload"]["type"] == "token_count" {
                let rl = &o["payload"]["rate_limits"];
                // primary가 채워진 이벤트만 유효 (간헐적으로 null로 기록됨)
                if !rl.is_null() && !rl["primary"].is_null() {
                    last = Some((
                        o["timestamp"].as_str().unwrap_or("").to_string(),
                        rl.clone(),
                    ));
                }
            }
        }
        if let Some((ts, rl)) = last {
            return Some(serde_json::json!({ "rate_limits": rl, "polled_at": ts }));
        }
    }
    None
}

/// 3분 캐시 — 프런트가 자주 불러도 API를 과도하게 치지 않음 (이 엔드포인트는
/// 짧은 간격으로 두드리면 429가 나기 쉬움).
static USAGE_CACHE: Mutex<Option<(std::time::Instant, serde_json::Value)>> = Mutex::new(None);

#[tauri::command]
fn subscription_state(force: bool) -> Option<serde_json::Value> {
    if !force {
        let cache = USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, v)) = cache.as_ref() {
            if t.elapsed().as_secs() < 180 {
                return Some(v.clone());
            }
        }
    }
    if let Some(direct) = fetch_usage_direct() {
        *USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), direct.clone()));
        return Some(direct);
    }
    // direct 호출 실패(429 등) 시, headroom의 오래됐을 수 있는 파일보다는
    // 직전에 성공했던 direct 응답(캐시 TTL을 넘겼더라도)을 우선한다 —
    // headroom 프로세스가 꺼져 있으면 그 파일이 며칠씩 묵어 있을 수 있음.
    {
        let cache = USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, v)) = cache.as_ref() {
            return Some(v.clone());
        }
    }
    usage_from_headroom()
}

/// headroom이 설치되어 있으면 절감 통계 반환 (없으면 None — 대시보드에서 섹션 생략)
#[tauri::command]
fn headroom_stats() -> Option<serde_json::Value> {
    let p = dirs::home_dir()?.join(".headroom").join("proxy_savings.json");
    let text = fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// 세션 프로젝트 폴더를 탐색기로 연다
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("폴더가 존재하지 않습니다: {}", path));
    }
    std::process::Command::new("explorer.exe")
        .arg(&p)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_session(file: String) -> Result<(), String> {
    // ".." 같은 성분이 섞이면 starts_with 검사를 그냥 통과하므로 반드시 정규화 후 비교
    let p = fs::canonicalize(&file).map_err(|e| e.to_string())?;
    let home = dirs::home_dir().ok_or("no home dir")?;
    // 알려진 세션 저장소 안의 세션 파일만 삭제 허용
    let allowed = [
        home.join(".claude").join("projects"),
        home.join(".codex").join("sessions"),
        home.join(".gemini").join("tmp"),
    ];
    let in_store = allowed
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .any(|root| p.starts_with(&root));
    let is_session = p
        .extension()
        .map(|e| e == "jsonl" || e == "json")
        .unwrap_or(false);
    if !in_store || !is_session {
        return Err("invalid session file path".into());
    }
    fs::remove_file(&p).map_err(|e| e.to_string())?;
    // 캐시 키는 스캔 당시의 원본 경로 문자열 (canonicalize한 \\?\ 형태가 아님)
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(&file);
    Ok(())
}

fn main() {
    install_panic_hook();
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            // VISIBLE 플래그 제외: 창 표시는 WebView 로드 후 프런트에서 수행 (IME 초기화 버그 회피)
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        - tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        .manage(PtyState::default())
        .invoke_handler(tauri::generate_handler![
            spawn_pty, write_pty, resize_pty, kill_pty, list_sessions, delete_session,
            session_preview, usage_stats, headroom_stats, subscription_state, codex_state,
            open_path
        ])
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let show = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("CLI Deck")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            spawn_state_monitor(app.handle().clone());

            // WebView2 초기 IME 바인딩 버그 우회: 시작 직후 포커스를 프로그램적으로
            // 재이동시켜 "다른 창 갔다 오기"와 동일한 재바인딩을 강제한다.
            // 이게 없으면 첫 입력에서 한글 조합이 중복되고 조합창이 화면 구석에 뜬다.
            if let Some(w) = app.get_webview_window("main") {
                let w2 = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(700));
                    let _ = w2.with_webview(|webview| unsafe {
                        use webview2_com::Microsoft::Web::WebView2::Win32::{
                            COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT,
                            COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
                        };
                        let controller = webview.controller();
                        let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT);
                        let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                    });
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // X 버튼 = 완전 종료. 창을 닫기 전에 열려 있는 PTY 자식 프로세스를
            // 먼저 정리해 고아 프로세스로 남지 않게 한다.
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let state = window.app_handle().state::<PtyState>();
                let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
                for (_, mut p) in map.drain() {
                    let _ = p.killer.kill();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running cli-deck");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 종료 감지의 근거 확인.
    /// ConPTY에서는 자식이 죽어도 마스터 쪽 read가 EOF를 돌려주지 않는다(측정 결과 10초 초과).
    /// 그래서 spawn_pty는 리더 EOF가 아니라 child.wait()로 종료를 판정한다 — 그 wait가
    /// 실제로 곧바로 돌아오는지 검증한다.
    #[test]
    fn child_wait_returns_when_process_exits() {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/c", "exit 1"]);
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        // 리더가 없으면 파이프가 막힐 수 있으므로 실사용과 동일하게 계속 비워준다
        let mut reader = pair.master.try_clone_reader().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
            "자식이 종료됐는데 wait()가 돌아오지 않음 — 탭이 '종료됨'으로 바뀌지 않는다"
        );
    }

    /// 완료 판정 게이트. 여기서 "진행 중"을 잘못 넓게 잡으면 탭이 영영
    /// 작업중으로 남으므로(방금 고친 종료 감지 버그의 거울상) 화이트리스트가
    /// 의도대로 좁은지 확인한다.
    #[test]
    fn turn_in_progress_whitelists_only_live_turns() {
        let dir = std::env::temp_dir().join("cli-deck-turn-test");
        let _ = fs::create_dir_all(&dir);
        let check = |name: &str, body: &str| {
            let p = dir.join(name);
            fs::write(&p, body).unwrap();
            turn_in_progress(&p)
        };

        // 툴 결과를 기다리는 중 = 진행 중
        assert!(check(
            "a.jsonl",
            r#"{"type":"assistant","message":{"stop_reason":"tool_use"}}"#
        ));
        // 프롬프트를 넣었고 아직 응답 없음 = 진행 중
        assert!(check(
            "b.jsonl",
            r#"{"type":"user","message":{"content":"안녕"}}"#
        ));
        // 정상 종료
        assert!(!check(
            "c.jsonl",
            r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#
        ));
        // 한도 초과 등으로 끝난 턴 — 열거하지 않아도 종료로 잡혀야 한다
        assert!(!check(
            "d.jsonl",
            r#"{"type":"assistant","message":{"stop_reason":"stop_sequence"}}"#
        ));
        // 사용자 중단 마커는 프롬프트가 아니다
        assert!(!check(
            "e.jsonl",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#
        ));
        // 서브에이전트 레코드가 메인 턴 상태를 덮어쓰면 안 된다
        assert!(!check(
            "f.jsonl",
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n\
             {\"type\":\"assistant\",\"isSidechain\":true,\"message\":{\"stop_reason\":\"tool_use\"}}"
        ));
        // 상태 레코드가 아닌 줄은 건너뛴다
        assert!(!check(
            "g.jsonl",
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n\
             {\"type\":\"file-history-snapshot\"}"
        ));
        // Gemini는 턴 단위 기록이 없어 파일로 판정할 수 없다 → 타임아웃에 맡긴다
        assert!(!check("h.json", r#"{"sessionId":"x","messages":[]}"#));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_ts_matches_known_epoch() {
        let got = parse_iso_ts("2026-07-04T17:22:51.651Z").unwrap();
        assert!((got - 1783185771.651).abs() < 0.001);
    }
}
