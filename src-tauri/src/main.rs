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
        atomic::{AtomicBool, AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::UNIX_EPOCH,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

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
/// 런타임 토글 — 설정 창의 체크박스로 켜고 끈다. 릴리스에서 컴파일로 빼버렸더니
/// 정작 문제가 보고되는 빌드에서 원인을 못 보는 상황이 생겨서 되돌렸다.
static PTY_TRACE: AtomicBool = AtomicBool::new(false);

/// 마커 파일이 있으면 재시작 후에도 켜진 상태가 유지된다 (환경변수는 그걸 설정한
/// 셸에서 띄웠을 때만 붙어서 놓치기 쉬움)
fn trace_marker_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("com.user.cli-deck").join("TRACE"))
}

fn init_trace() {
    let by_env = std::env::var("CLI_DECK_PTY_TRACE")
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0"
        })
        .unwrap_or(false);
    let by_file = trace_marker_path().map(|p| p.exists()).unwrap_or(false);
    PTY_TRACE.store(by_env || by_file, Ordering::Relaxed);
}

/// 진단 파일 정리. 지울 파일을 이름으로 명시한다 — 이 폴더에는 WebView2 프로필
/// (EBWebView, 앱 설정이 들어 있는 localStorage)이 같이 있어서 폴더째 지우면
/// 사용자 설정이 통째로 날아간다.
#[tauri::command]
fn clear_diagnostics() -> Result<String, String> {
    // 기록 스레드는 파일이 사라지면 다음 배치에서 스스로 다시 연다.
    let dir = dirs::data_local_dir()
        .map(|d| d.join("com.user.cli-deck"))
        .ok_or("데이터 폴더를 찾을 수 없습니다")?;
    let mut freed = 0u64;
    let mut names: Vec<String> = Vec::new();
    for name in ["pty-trace.log", "pty-trace.prev.log", "crash.log"] {
        let p = dir.join(name);
        let Ok(meta) = fs::metadata(&p) else { continue };
        let len = meta.len();
        if fs::remove_file(&p).is_ok() {
            freed += len;
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        return Ok("지울 파일이 없습니다".into());
    }
    Ok(format!(
        "{} 삭제 · {:.1}MB 정리",
        names.join(", "),
        freed as f64 / (1024.0 * 1024.0)
    ))
}

/// 프런트엔드가 재는 값을 같은 트레이스 파일로 넘긴다. 입력이 늦다가 몰려 보이는
/// 증상의 원인이 사이드바 재구축인지 터미널 페인트인지, 추론 대신 구분하기 위한 것.
#[tauri::command]
fn trace_ui(kind: String, value: String) {
    trace("ui", "webview", &kind, &value);
}

#[tauri::command]
fn trace_enabled() -> bool {
    PTY_TRACE.load(Ordering::Relaxed)
}

/// 설정에서 켜고 끄기. 마커 파일로 상태를 남겨 재시작 후에도 유지된다.
/// 반환값은 로그 파일 경로 (설정 창에 표시).
#[tauri::command]
fn set_trace(enabled: bool) -> Result<String, String> {
    PTY_TRACE.store(enabled, Ordering::Relaxed);
    if let Some(p) = trace_marker_path() {
        if enabled {
            if let Some(dir) = p.parent() {
                fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            fs::write(&p, b"").map_err(|e| e.to_string())?;
        } else {
            let _ = fs::remove_file(&p);
        }
    }
    Ok(trace_marker_path()
        .and_then(|p| p.parent().map(|d| d.join("pty-trace.log").to_string_lossy().to_string()))
        .unwrap_or_default())
}

static TRACE_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// 며칠 켜둬도 디스크를 잡아먹지 않도록. 샘플링 대신 상한으로 끊는다 —
/// 청크 간격 분포가 신호 자체라 솎아내면 데이터가 망가진다.
const TRACE_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// ccmanager가 Claude 감지에 쓰는 스피너 문자 집합
const SPINNER_GLYPHS: &str = "✱✲✳✴✵✶✷✸✹✺✻✼✽✾✿❀❁❂❃❇❈❉❊❋✢✣✤✥✦✧✨⊛⊕⊙◉◎◍⁂⁕※⍟☼★☆·•⏺▸▹∙⋅○●";

// 기록은 별도 스레드가 맡는다. 예전엔 청크마다 리더 스레드에서 바로 파일에 썼는데,
// 실측 결과 초당 137번의 쓰기 시스템 콜이 났다 — 그 스레드는 터미널 출력을 배달하는
// 스레드라, 파일 시스템이나 백신이 한 번 붙들면 그동안 화면이 멈춘다.
// 이제 핫패스는 채널에 문자열 하나 보내고 끝이고, 실제 쓰기는 모아서 처리한다.
static TRACE_TX: LazyLock<std::sync::mpsc::Sender<String>> = LazyLock::new(|| {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut sink: Option<(fs::File, u64)> = None;
        while let Ok(first) = rx.recv() {
            // 깨어난 김에 쌓인 것을 모두 모아 한 번에 쓴다
            let mut batch = first;
            while let Ok(more) = rx.try_recv() {
                batch.push_str(&more);
            }
            let Some(path) = trace_log_path() else { continue };
            // 밖에서 파일이 지워졌으면 핸들을 버리고 다시 연다 (안 그러면 삭제된
            // 파일에 계속 쓰게 되어 기록이 조용히 사라진다)
            if sink.is_some() && !path.exists() {
                sink = None;
            }
            if sink.is_none() {
                let Some(dir) = path.parent() else { continue };
                if fs::create_dir_all(dir).is_err() {
                    continue;
                }
                let Ok(f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
                    continue;
                };
                let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                sink = Some((f, size));
            }
            if let Some((f, written)) = sink.as_mut() {
                if *written >= TRACE_MAX_BYTES {
                    continue;
                }
                use std::io::Write as _;
                if f.write_all(batch.as_bytes()).is_ok() {
                    *written += batch.len() as u64;
                }
            }
        }
    });
    tx
});

fn trace_log_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("com.user.cli-deck").join("pty-trace.log"))
}

fn trace(id: &str, agent: &str, kind: &str, value: &str) {
    if !PTY_TRACE.load(Ordering::Relaxed) {
        return;
    }
    let _ = TRACE_TX.send(format!(
        "{}	{}	{}	{}	{}
",
        TRACE_START.elapsed().as_millis(),
        id,
        agent,
        kind,
        value
    ));
}

/// 트레이스에 남기기 전 환경변수 값을 가린다. 설정의 "전역 환경변수"는
/// `set KEY=VAL&&` 형태로 실행 명령 앞에 붙는데, 거기 토큰을 넣어 쓰는 사용법이
/// 있어서 그대로 적으면 로그 파일에 비밀값이 평문으로 남는다.
/// 과하게 가려지는 편이 안전하므로 "set X=" 패턴은 모두 마스킹한다.
fn redact_env(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd;
    while let Some(p) = rest.find("set ") {
        out.push_str(&rest[..p + 4]);
        rest = &rest[p + 4..];
        let Some(eq) = rest.find('=') else { break };
        out.push_str(&rest[..eq]);
        out.push_str("=***");
        rest = match rest[eq..].find("&&") {
            Some(amp) => &rest[eq + amp..],
            None => "",
        };
    }
    out.push_str(rest);
    out
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


// ---------- statusLine 탭 ----------
// Claude Code는 상태줄 명령에 세션 JSON을 stdin으로 넘긴다. 거기엔 요금제 한도,
// 실제 컨텍스트 윈도우 크기, 실제 비용이 들어 있다 — 우리가 지금 API 호출이나
// 추측으로 얻는 값들이다. 그 흐름을 옆으로 복사해 두고, 화면 출력은 원래대로
// 통과시킨다(사용자가 쓰던 상태줄이 있으면 그걸 실행해 그대로 넘긴다).
//
// 사용자의 ~/.claude/settings.json은 읽기만 하고 절대 수정하지 않는다.
// 우리 설정은 별도 파일로 두고 spawn 시 --settings 로 넘긴다.

fn status_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("com.user.cli-deck").join("status"))
}

/// 사용자가 원래 쓰던 상태줄 명령 (없으면 None). 우리 자신은 걸러 재귀를 막는다.
fn user_statusline_command() -> Option<String> {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"));
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(base.join("settings.json")).ok()?).ok()?;
    let cmd = v["statusLine"]["command"].as_str()?.trim().to_string();
    if cmd.is_empty() || cmd.contains(STATUSLINE_FLAG) {
        return None;
    }
    Some(cmd)
}

const STATUSLINE_FLAG: &str = "--statusline-tap";

/// GUI를 띄우지 않고 stdin만 처리하고 끝나는 모드
fn run_statusline_tap() {
    use std::io::{Read as _, Write as _};
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    // 세션별로 저장 — 탭과 1:1로 매칭된다
    if let (Some(dir), Ok(v)) = (
        status_dir(),
        serde_json::from_str::<serde_json::Value>(&input),
    ) {
        if let Some(sid) = v["session_id"].as_str() {
            if fs::create_dir_all(&dir).is_ok() {
                let _ = fs::write(dir.join(format!("{}.json", sid)), &input);
            }
        }
    }
    // 화면은 원래대로: 사용자 명령이 있으면 같은 stdin으로 실행해 출력을 그대로 넘긴다
    if let Some(cmd) = user_statusline_command() {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("cmd.exe")
            .args(["/c", &cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(input.as_bytes());
            }
            if let Ok(out) = child.wait_with_output() {
                let _ = std::io::stdout().write_all(&out.stdout);
            }
        }
    }
}

/// 세션의 최신 상태줄 페이로드
fn read_status(session_id: &str) -> Option<serde_json::Value> {
    let p = status_dir()?.join(format!("{}.json", session_id));
    serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
}

/// 가장 최근에 갱신된 상태줄 페이로드의 요금제 한도.
/// 너무 오래된 값은 쓰지 않는다(세션이 다 닫혀 있으면 갱신이 멈춘다).
fn statusline_rate_limits() -> Option<serde_json::Value> {
    let dir = status_dir()?;
    let mut newest: Option<(f64, PathBuf)> = None;
    for e in fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        let t = file_mtime(&p);
        if newest.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
            newest = Some((t, p));
        }
    }
    let (mtime, path) = newest?;
    let age = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64()
        - mtime;
    if age > 600.0 {
        return None; // 10분 넘게 안 갱신됐으면 신뢰하지 않는다
    }
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let rl = &v["rate_limits"];
    if rl["five_hour"].is_null() {
        return None;
    }
    let map = |w: &serde_json::Value| {
        serde_json::json!({
            "utilization_pct": w["used_percentage"],
            "resets_at": w["resets_at"].as_i64().map(|t| t * 1000),
        })
    };
    Some(serde_json::json!({
        "source": "statusline",
        "five_hour": map(&rl["five_hour"]),
        "seven_day": map(&rl["seven_day"]),
        "polled_at": chrono_now_iso(),
    }))
}

/// CLI Deck 전용 설정 파일을 만들고 경로를 돌려준다. 이 경로를 spawn 시
/// --settings 로 넘기면 사용자 settings.json을 건드리지 않고 상태줄만 얹는다.
#[tauri::command]
fn statusline_settings_path() -> Result<String, String> {
    let dir = dirs::data_local_dir()
        .map(|d| d.join("com.user.cli-deck"))
        .ok_or("데이터 폴더를 찾을 수 없습니다")?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cfg = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("\"{}\" {}", exe.to_string_lossy(), STATUSLINE_FLAG),
        }
    });
    let p = dir.join("statusline-settings.json");
    fs::write(&p, serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(p.to_string_lossy().to_string())
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

// ---------- 프롬프트 캐시 유지 (keep-alive) ----------
// 캐시는 읽을 때마다 TTL이 갱신되고, 읽기는 입력가의 0.1×인 반면 1시간 캐시를
// 다시 쓰는 건 2×다. 만료 직전에 한 번 읽어주면 20배 싸게 유지된다.
// 기본은 꺼져 있다 — 실제로 메시지를 보내고 돈을 쓰는 기능이라 명시적 opt-in.

/// 핑 전송 후 다음 핑까지 최소 대기. 자기가 보낸 핑의 효과가 세션 파일에
/// 반영되기 전에 또 쏘는 걸 막는다 (창이 최소화되면 목록 폴링이 멈춰서
/// cache_last_ts 갱신이 늦어질 수 있음).
fn keepalive_cooldown_ms(ttl_secs: u32) -> u64 {
    (u64::from(ttl_secs) * 1000 / 2).max(60_000)
}
/// 최근 이만큼 안에 사용자 입력이 있었으면 건너뛴다 (타이핑 중 끼어들기 방지)
const KEEPALIVE_INPUT_QUIET_MS: u64 = 60_000;
/// 첫 핑으로부터 이 시간이 지나면 중단. 횟수가 아니라 경과 시간으로 끊는다 —
/// 건너뛴 회차가 있으면 횟수 상한은 얼마든지 늘어난다.
const KEEPALIVE_MAX_SPAN_MS: u64 = 8 * 60 * 60 * 1000;
/// 이보다 작은 컨텍스트는 유지할 가치가 없다
const KEEPALIVE_MIN_CTX: u64 = 20_000;
/// Enter 이후 입력창이 비워지고 다시 그려질 때까지 기다리는 시간
const KEEPALIVE_RESTORE_DELAY_MS: u64 = 400;

#[derive(Clone)]
struct KeepAlive {
    enabled: bool,
    /// 남은 TTL이 이보다 적으면 핑
    threshold_secs: u64,
    message: String,
}

static KEEPALIVE: LazyLock<Mutex<KeepAlive>> = LazyLock::new(|| {
    Mutex::new(KeepAlive {
        enabled: false,
        threshold_secs: 120,
        message: "reply \".\" only".into(),
    })
});

#[tauri::command]
fn set_keepalive(enabled: bool, threshold_secs: u64, message: String) {
    let mut k = KEEPALIVE.lock().unwrap_or_else(|e| e.into_inner());
    k.enabled = enabled;
    k.threshold_secs = threshold_secs.clamp(30, 3600);
    if !message.trim().is_empty() {
        k.message = message.trim().to_string();
    }
}

struct Activity {
    last_input: Option<std::time::Instant>,
    last_out: Option<std::time::Instant>,
    burst_start: Option<std::time::Instant>,
    working: bool,
    last_check: Option<std::time::Instant>,
    agent: String,
    /// 알림 문구용 탭 제목
    title: String,
    /// 세션 파일 — 턴 종료 판정용. 없으면(새 세션) 침묵 타임아웃만 쓴다.
    file: Option<PathBuf>,
    /// 마지막 제출 이후 뭔가 입력됨 (내용은 몰라도 됨 — 있는지만 알면 된다)
    draft: bool,
    /// 이스케이프 시퀀스 파싱 상태 (0=없음, 1=ESC 직후, 2=CSI 안)
    esc_state: u8,
    /// draft 안의 줄바꿈 수 (Alt+Enter 또는 붙여넣기). Ctrl+U 횟수 계산에 쓴다.
    draft_lines: u32,
    /// 괄호 붙여넣기(ESC[200~ … ESC[201~) 안인지. 붙여넣은 개행은 제출이 아니다.
    in_paste: bool,
    /// CSI 파라미터 앞 3바이트 — 붙여넣기 마커(200/201) 판별용
    csi: [u8; 3],
    csi_len: u8,
    last_ping: Option<std::time::Instant>,
    first_ping: Option<std::time::Instant>,
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


/// 캐시 유지 핑 전송. 사용자가 쓰다 만 입력은 에이전트의 kill ring에 맡겼다가
/// 되돌린다 — 우리가 내용을 추적하면 IME·여러 줄·히스토리를 전부 따라가야 하는데,
/// Ctrl+U/Ctrl+Y는 에이전트 자신이 원문을 보관하므로 그럴 필요가 없다.
///   스페이스 먼저 — 입력이 비어 있어도 kill ring에 이번 내용이 확실히 들어가고,
///                   빈 프롬프트에서 Ctrl+U가 다른 동작을 하는 것도 막는다
///   Ctrl+U(0x15)  줄 전체를 kill ring으로
///   메시지 + Enter
///   Ctrl+Y(0x19)  원문 복원
///   Backspace(0x7f)  위에서 넣은 스페이스 제거
fn send_keepalive(app: AppHandle, id: String, agent: String, message: String, draft: bool) {
    std::thread::spawn(move || {
        let write = |bytes: &[u8]| {
            let state = app.state::<PtyState>();
            let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
            match map.get_mut(&id) {
                Some(p) => p.writer.write_all(bytes).is_ok(),
                None => false, // 탭이 닫혔다
            }
        };
        // 스페이스 → Ctrl+U 로 한 줄을 kill ring에 옮긴다. 스페이스를 먼저 넣는 건
        // 입력이 비어 있어도 kill ring에 이번 내용이 확실히 들어가게 하고, 빈 프롬프트에서
        // Ctrl+U가 다른 동작을 하는 것도 막기 위해서다. 여러 줄 draft는 애초에 보내지
        // 않으므로(keepalive_pass) 한 번이면 충분하다.
        if !write(&[b' ', 0x15]) {
            return;
        }
        if !write(format!("{}\r", message).as_bytes()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(KEEPALIVE_RESTORE_DELAY_MS));
        write(&[0x19, 0x7f]);
        trace(&id, &agent, "keepalive", &format!("{} (draft={})", message, draft));
    });
}

/// 남은 캐시 TTL(초)과 티어. 세션 파일에서 직접 읽으므로 프런트 폴링 상태와 무관하다.
fn cache_ttl_remaining(file: &PathBuf) -> Option<(f64, u32)> {
    let meta = cached_meta_light(file, parser_for(file))?;
    let last = meta.cache_last_ts?;
    let ttl = meta.cache_ttl_secs?;
    if meta.ctx_tokens.unwrap_or(0) < KEEPALIVE_MIN_CTX {
        return None; // 작은 세션은 유지할 가치가 없다
    }
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some((last + f64::from(ttl) - now, ttl))
}

/// 사용자가 입력창에 뭔가 써 뒀는지 추적한다. 내용은 알 필요 없고 있는지만 알면 된다.
///
/// 터미널은 앱의 질의에 이스케이프 시퀀스로 **자동 응답**한다(커서 위치 보고 등).
/// 그 응답에도 숫자·문자가 들어 있어서 단순히 "출력 가능 문자 = 타이핑"으로 세면
/// 자리를 비운 사이에도 draft가 참으로 굳는다 — 실측에서 85분 동안 427건이 들어왔다.
/// 그래서 시퀀스를 건너뛴 뒤에 남는 문자만 입력으로 인정한다.
fn note_draft(a: &mut Activity, bytes: &[u8]) {
    for &b in bytes {
        match a.esc_state {
            1 => {
                // Alt+Enter(ESC+CR)는 제출이 아니라 입력창 안의 줄바꿈이다
                if b == 13 || b == 10 {
                    a.draft = true;
                    a.draft_lines += 1;
                }
                a.esc_state = if b == b'[' { 2 } else { 0 };
            }
            2 => {
                if (0x40..=0x7e).contains(&b) {
                    // 괄호 붙여넣기 시작/끝 마커 (ESC[200~ / ESC[201~)
                    if b == b'~' && a.csi_len == 3 {
                        if &a.csi == b"200" {
                            a.in_paste = true;
                        } else if &a.csi == b"201" {
                            a.in_paste = false;
                        }
                    }
                    a.esc_state = 0; // CSI 종료 바이트
                    a.csi_len = 0;
                } else if (a.csi_len as usize) < a.csi.len() {
                    a.csi[a.csi_len as usize] = b;
                    a.csi_len += 1;
                }
            }
            _ => match b {
                0x1b => a.esc_state = 1,
                // 붙여넣은 개행은 제출이 아니라 입력창 안의 줄바꿈이다
                13 | 10 if a.in_paste => {
                    a.draft = true;
                    a.draft_lines += 1;
                }
                13 | 10 | 3 | 0x15 => {
                    a.draft = false; // Enter(제출) / Ctrl+C / Ctrl+U
                    a.draft_lines = 0;
                    a.in_paste = false;
                }
                0x20..=0x7e | 0x80..=0xff => a.draft = true,
                _ => {}
            },
        }
    }
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
    std::thread::spawn(move || {
        let mut tick: u64 = 0;
        loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        tick = tick.wrapping_add(1);
        let now = std::time::Instant::now();
        let ms = |a: std::time::Instant, b: std::time::Instant| b.duration_since(a).as_millis() as u64;

        // 캐시 유지 검사 — 30초에 한 번이면 2분 임계에 충분하다
        if tick % 120 == 0 {
            keepalive_pass(&app, now);
        }

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
            // 창이 최소화·백그라운드면 WebView2가 렌더러를 재워서 JS 리스너가 돌지
            // 않는다 — 알림이 가장 필요한 순간이 정확히 그때이므로 여기서 직접 보낸다.
            // 포커스가 있을 때는 JS가 앱 내 토스트를 띄우므로 중복되지 않는다.
            let focused = app
                .get_webview_window("main")
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false);
            let (agent, title) = {
                let act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
                let a = act.get(&id);
                (
                    a.map(|a| a.agent.clone()).unwrap_or_default(),
                    a.map(|a| a.title.clone()).unwrap_or_default(),
                )
            };
            if focused {
                trace(&id, &agent, "notify", "skip:focused");
            } else {
                let r = app
                    .notification()
                    .builder()
                    .title("✻ 응답 완료")
                    .body(if title.is_empty() { "세션" } else { &title })
                    .show();
                // 알림이 안 뜬다는 신고가 있었는데 릴리스 빌드엔 계측이 없어 원인이
                // 안 보였다. 전이·포커스·전송 결과를 남겨 다음엔 로그로 판별한다.
                trace(
                    &id,
                    &agent,
                    "notify",
                    &match r {
                        Ok(()) => "sent".to_string(),
                        Err(e) => format!("err:{}", e),
                    },
                );
            }
            let _ = app.emit("pty-state", PtyStateEvent { id, working: false });
        }
        }
    });
}

/// 만료가 임박한 세션에 캐시 유지 핑을 보낸다.
fn keepalive_pass(app: &AppHandle, now: std::time::Instant) {
    let cfg = KEEPALIVE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !cfg.enabled {
        return;
    }
    let ms = |a: std::time::Instant| now.duration_since(a).as_millis() as u64;

    // 잠금 안에서는 후보만 고른다 (파일 파싱은 잠금 밖에서)
    let mut candidates: Vec<(String, String, PathBuf, bool, u32)> = Vec::new();
    {
        let act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
        for (id, a) in act.iter() {
            let Some(file) = a.file.clone() else { continue };
            if a.working {
                continue; // 작업 중이면 애초에 캐시가 살아 있다
            }
            // 쓰다 만 입력이 있어도 보낸다 — 전송 시퀀스가 Ctrl+U로 kill ring에
            // 옮겼다가 Ctrl+Y로 되돌린다. draft 여부는 트레이스에만 남긴다.
            if a.last_input.map(|t| ms(t) < KEEPALIVE_INPUT_QUIET_MS).unwrap_or(false) {
                continue; // 방금 타이핑했다
            }
            if a.first_ping.map(|t| ms(t) > KEEPALIVE_MAX_SPAN_MS).unwrap_or(false) {
                continue; // 너무 오래 자리를 비웠다 — 그만 유지한다
            }
            candidates.push((id.clone(), a.agent.clone(), file, a.draft, a.draft_lines));
        }
    }

    for (id, agent, file, draft, _lines) in candidates {
        let Some((remain, ttl)) = cache_ttl_remaining(&file) else { continue };
        if remain <= 0.0 || remain > cfg.threshold_secs as f64 {
            continue; // 이미 만료됐거나 아직 여유 있음
        }
        let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
        let Some(a) = act.get_mut(&id) else { continue };
        // 자기가 보낸 핑이 파일에 반영되기 전에 또 쏘지 않도록
        if a.last_ping.map(|t| ms(t) < keepalive_cooldown_ms(ttl)).unwrap_or(false) {
            continue;
        }
        a.last_ping = Some(now);
        a.first_ping.get_or_insert(now);
        drop(act);
        send_keepalive(app.clone(), id, agent, cfg.message.clone(), draft);
    }
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
    // 알림에 쓸 탭 제목
    title: Option<String>,
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
    trace(&id, &agent, "spawn", &redact_env(&claude_cmd));

    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).insert(
        id.clone(),
        Activity {
            last_input: None,
            last_out: None,
            burst_start: None,
            working: false,
            last_check: None,
            agent: agent.clone(),
            title: title.unwrap_or_default(),
            draft: false,
            esc_state: 0,
            draft_lines: 0,
            in_paste: false,
            csi: [0; 3],
            csi_len: 0,
            last_ping: None,
            first_ping: None,
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
                    if PTY_TRACE.load(Ordering::Relaxed) {
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
            note_draft(a, data.as_bytes());
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
    /// 백그라운드 에이전트 상태 (working/blocked/failed) — 아니면 None
    bg_state: Option<String>,
    /// 백그라운드 에이전트가 지금 뭘 하고 있는지 한 줄
    bg_detail: Option<String>,
    /// 데몬 로스터에 살아 있는가 (죽은 bg 세션과 구분)
    bg_running: bool,
    /// 이 세션이 포크돼 나온 원본 세션 ID (실행 중일 때만 알 수 있음)
    parent_id: Option<String>,
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
        bg_state: m.bg_state.clone(),
        bg_detail: m.bg_detail.clone(),
        bg_running: m.bg_running,
        parent_id: m.parent_id.clone(),
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
        bg_state: None,
        bg_detail: None,
        bg_running: false,
        parent_id: None,
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
        bg_state: None,
        bg_detail: None,
        bg_running: false,
        parent_id: None,
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
        bg_state: None,
        bg_detail: None,
        bg_running: false,
        parent_id: None,
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

/// Claude Code 백그라운드 에이전트 정보. `~/.claude/jobs/<8자리>/state.json` 규칙에 따라
/// 세션 ID 앞 8자리를 키로 쓴다.
struct BgInfo {
    state: String,
    detail: String,
    running: bool,
    parent_id: Option<String>,
}

fn scan_bg_jobs() -> HashMap<String, BgInfo> {
    let mut out: HashMap<String, BgInfo> = HashMap::new();
    let Some(home) = dirs::home_dir() else { return out };

    // 살아 있는 워커만 로스터에 남는다. 포크 출처(원본 세션)도 여기서만 알 수 있다.
    let mut running: HashMap<String, Option<String>> = HashMap::new();
    if let Ok(text) = fs::read_to_string(home.join(".claude").join("daemon").join("roster.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(ws) = v["workers"].as_object() {
                for (short, w) in ws {
                    let parent = w["dispatch"]["launch"]["sessionId"].as_str().and_then(|p| {
                        std::path::Path::new(p)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                    });
                    running.insert(short.clone(), parent);
                }
            }
        }
    }

    let Ok(entries) = fs::read_dir(home.join(".claude").join("jobs")) else { return out };
    for e in entries.flatten() {
        let short = e.file_name().to_string_lossy().to_string();
        let Ok(text) = fs::read_to_string(e.path().join("state.json")) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let run = running.get(&short);
        out.insert(
            short,
            BgInfo {
                state: v["state"].as_str().unwrap_or("unknown").to_string(),
                detail: v["detail"].as_str().unwrap_or("").to_string(),
                running: run.is_some(),
                parent_id: run.and_then(|p| p.clone()),
            },
        );
    }
    out
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
    // 상태줄이 실제 컨텍스트 윈도우를 알려준다 — 모델명으로 추측하던 걸 대체한다
    for m in out.iter_mut() {
        let Some(v) = read_status(&m.session_id) else { continue };
        if let Some(size) = v["context_window"]["context_window_size"].as_u64() {
            m.ctx_window = Some(size);
        }
        if let Some(used) = v["context_window"]["total_input_tokens"].as_u64() {
            if used > 0 {
                m.ctx_tokens = Some(used);
            }
        }
    }

    // 백그라운드 에이전트 상태 붙이기 (세션 ID 앞 8자리로 매칭)
    let bg = scan_bg_jobs();
    for m in out.iter_mut() {
        let Some(short) = m.session_id.get(..8) else { continue };
        let Some(info) = bg.get(short) else { continue };
        m.bg_state = Some(info.state.clone());
        m.bg_running = info.running;
        if !info.detail.is_empty() {
            m.bg_detail = Some(info.detail.clone());
        }
        m.parent_id = info.parent_id.clone();
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
    // 상태줄이 살아 있으면 그 값을 쓴다 — API 호출도 토큰도 필요 없고 429도 없다
    if let Some(sl) = statusline_rate_limits() {
        return Some(sl);
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

/// 세션 로그 파일(JSONL)을 기본 연결 프로그램으로 연다.
/// 연결 프로그램이 없으면 Windows가 "연결 프로그램 선택" 창을 띄운다 —
/// spawn 자체는 성공하므로 프런트의 catch로는 그 경우를 알 수 없다.
#[tauri::command]
fn open_log_file(file: String) -> Result<(), String> {
    let p = session_file_in_store(&file)?;
    std::process::Command::new("explorer.exe")
        .arg(&p)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 알려진 세션 저장소 안의 세션 파일인지 검사하고 정규화된 경로를 돌려준다.
/// ".." 같은 성분이 섞이면 starts_with 검사를 그냥 통과하므로 반드시 정규화 후 비교.
fn session_file_in_store(file: &str) -> Result<PathBuf, String> {
    let p = fs::canonicalize(file).map_err(|e| e.to_string())?;
    let home = dirs::home_dir().ok_or("no home dir")?;
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
    Ok(p)
}

#[tauri::command]
fn delete_session(file: String) -> Result<(), String> {
    // 알려진 세션 저장소 안의 세션 파일만 삭제 허용
    let p = session_file_in_store(&file)?;
    fs::remove_file(&p).map_err(|e| e.to_string())?;
    // 캐시 키는 스캔 당시의 원본 경로 문자열 (canonicalize한 \\?\ 형태가 아님)
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(&file);
    Ok(())
}

fn main() {
    // 상태줄 명령으로 불린 경우 GUI를 띄우지 않고 stdin만 처리한다
    if std::env::args().any(|a| a == STATUSLINE_FLAG) {
        run_statusline_tap();
        return;
    }
    install_panic_hook();
    init_trace();
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
            session_preview, trace_enabled, set_trace, clear_diagnostics, set_keepalive, statusline_settings_path, trace_ui, usage_stats, headroom_stats, subscription_state, codex_state, open_log_file,
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

    /// Alt+Enter(ESC+CR)는 제출이 아니라 입력창 안의 줄바꿈이다.
    /// 줄 수가 1을 넘으면 keepalive_pass가 전송을 건너뛴다 — Ctrl+U 한 번으로는
    /// 다 지워지지 않아 남은 앞줄이 핑과 함께 제출되기 때문이다.
    #[test]
    fn alt_enter_counts_lines_without_submitting() {
        let mut a = blank_activity();
        note_draft(&mut a, b"first");
        note_draft(&mut a, b"\x1b\r"); // Alt+Enter
        note_draft(&mut a, b"second");
        note_draft(&mut a, b"\x1b\r");
        note_draft(&mut a, b"third");
        assert!(a.draft);
        assert_eq!(a.draft_lines, 2); // 줄바꿈 2번 = 세 줄


        // 진짜 Enter로 제출하면 초기화된다
        note_draft(&mut a, b"\r");
        assert!(!a.draft);
        assert_eq!(a.draft_lines, 0);
    }

    /// 여러 줄을 붙여넣으면 개행이 ESC 없이 맨 CR로 들어온다. 제출로 오인하면
    /// 줄 수가 0으로 리셋되어 Ctrl+U를 한 번만 보내고 앞 줄들이 그대로 전송된다.
    #[test]
    fn pasted_newlines_count_as_lines_not_submits() {
        let mut a = blank_activity();
        note_draft(&mut a, b"\x1b[200~one\rtwo\rthree\x1b[201~");
        assert!(a.draft);
        assert_eq!(a.draft_lines, 2); // 개행 2번 = 세 줄


        // 붙여넣기가 끝난 뒤의 Enter는 진짜 제출이다
        note_draft(&mut a, b"\r");
        assert!(!a.draft);
        assert_eq!(a.draft_lines, 0);

        // 붙여넣기 밖의 개행은 여전히 제출
        let mut b = blank_activity();
        note_draft(&mut b, b"hello\r");
        assert!(!b.draft);
    }

    fn blank_activity() -> Activity {
        Activity {
            last_input: None,
            last_out: None,
            burst_start: None,
            working: false,
            last_check: None,
            agent: String::new(),
            title: String::new(),
            draft: false,
            esc_state: 0,
            draft_lines: 0,
            in_paste: false,
            csi: [0; 3],
            csi_len: 0,
            last_ping: None,
            first_ping: None,
            file: None,
        }
    }

    fn draft_after(chunks: &[&[u8]]) -> bool {
        let mut a = blank_activity();
        for c in chunks {
            note_draft(&mut a, c);
        }
        a.draft
    }

    /// 캐시 유지가 한 번도 안 나갔던 원인. 터미널은 앱의 질의에 이스케이프 시퀀스로
    /// 자동 응답하는데(실측: 자리 비운 85분 동안 427건), 그 안의 숫자·문자를 타이핑으로
    /// 세면 draft가 영구히 참이 되어 핑이 계속 건너뛰어진다.
    #[test]
    fn draft_ignores_terminal_escape_replies() {
        // 커서 위치 보고 — 사용자 입력이 아니다
        assert!(!draft_after(&[b"\x1b[45;12R"]));
        // 여러 건이 연달아 와도 마찬가지
        assert!(!draft_after(&[b"\x1b[45;12R", b"\x1b[1;1R", b"\x1b[?1;2c"]));
        // 시퀀스가 청크 경계에서 잘려도 상태가 이어져야 한다
        assert!(!draft_after(&[b"\x1b[45", b";12R"]));
        // 실제 타이핑은 잡는다
        assert!(draft_after(&[b"hello"]));
        // 한글(멀티바이트)도 잡는다
        assert!(draft_after(&["안녕".as_bytes()]));
        // Enter로 제출하면 해제
        assert!(!draft_after(&[b"hello", b"\r"]));
        // Ctrl+U로 지워도 해제
        assert!(!draft_after(&[b"hello", &[0x15]]));
        // 붙여넣기: 마커는 무시하고 내용은 입력으로 인정
        assert!(draft_after(&[b"\x1b[200~pasted\x1b[201~"]));
        // 시퀀스 뒤에 진짜 타이핑이 이어지면 잡는다
        assert!(draft_after(&[b"\x1b[45;12R", b"a"]));
    }

    #[test]
    fn redact_env_hides_values_not_command() {
        assert_eq!(
            redact_env("set TOKEN=sk-secret&&set B=2&&claude --resume abc"),
            "set TOKEN=***&&set B=***&&claude --resume abc"
        );
        // 환경변수가 없으면 그대로
        assert_eq!(redact_env("claude --resume abc"), "claude --resume abc");
        // 값에 &&가 없는 마지막 항목도 가려져야 한다
        assert_eq!(redact_env("set K=v"), "set K=***");
    }

    #[test]
    fn iso_ts_matches_known_epoch() {
        let got = parse_iso_ts("2026-07-04T17:22:51.651Z").unwrap();
        assert!((got - 1783185771.651).abs() < 0.001);
    }
}
