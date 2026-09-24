// 작업 상태 판정과 프롬프트 캐시 유지

use super::*;

// ---------- 작업 상태 판정 ----------
// 전부 pty-trace.log 실측으로 정한 값이다. 근거는 각 상수에 적어둔다.
// 핵심 구조: "지금 작업 중인가"는 PTY 출력 밀도가 답하고(Rust에서 판정하므로
// WebView 백그라운드 스로틀링과 무관), "턴이 끝났는가"는 세션 파일이 답한다.
// 침묵 길이로 완료를 판정할 수 없다는 게 실측으로 확인됐다 — 턴 내부 침묵이
// 78.7초까지 관측된 반면 턴 종료 침묵이 28.7초인 사례가 있어 분포가 역전된다.

/// 입력 후 이 시간 안에 온 출력은 타이핑 에코로 보고 활동에서 제외.
/// 실측: 300~1200ms 중 어느 값을 써도 결과가 같았다(민감하지 않음).
pub(crate) const ECHO_MS: u64 = 800;
/// 이보다 벌어지면 다른 버스트. 실측: 작업 중 청크 간격 p90 = 145ms.
pub(crate) const BURST_GAP_MS: u64 = 500;
/// 버스트가 이보다 길면 작업 중. 실측: 대기 중 단발 출력은 최대 250ms,
/// 가장 짧은 에이전트 버스트는 1.15초로 사이가 비어 있다.
pub(crate) const BURST_MIN_MS: u64 = 1000;
/// 파일이 "아직 작업 중"이라고 말해도 이만큼 조용하면 강제로 완료 처리.
/// 세션 파일이 없는 탭(새 세션·Gemini)의 유일한 완료 판정이기도 하다.
/// 실측된 턴 내부 최장 침묵 78.7초에 여유를 둔 값.
pub(crate) const MAX_WORKING_SILENCE_MS: u64 = 120_000;
/// 버스트가 끝난 뒤 파일을 다시 확인하는 간격 (250ms마다 읽지 않도록)
pub(crate) const FILE_RECHECK_MS: u64 = 2_000;

// ---------- 프롬프트 캐시 유지 (keep-alive) ----------
// 캐시는 읽을 때마다 TTL이 갱신되고, 읽기는 입력가의 0.1×인 반면 1시간 캐시를
// 다시 쓰는 건 2×다. 만료 직전에 한 번 읽어주면 20배 싸게 유지된다.
// 기본은 꺼져 있다 — 실제로 메시지를 보내고 돈을 쓰는 기능이라 명시적 opt-in.

/// 핑 전송 후 다음 핑까지 최소 대기. 자기가 보낸 핑의 효과가 세션 파일에
/// 반영되기 전에 또 쏘는 걸 막는다 (창이 최소화되면 목록 폴링이 멈춰서
/// cache_last_ts 갱신이 늦어질 수 있음).
pub(crate) fn keepalive_cooldown_ms(ttl_secs: u32) -> u64 {
    (u64::from(ttl_secs) * 1000 / 2).max(60_000)
}
/// 최근 이만큼 안에 사용자 입력이 있었으면 건너뛴다 (타이핑 중 끼어들기 방지)
pub(crate) const KEEPALIVE_INPUT_QUIET_MS: u64 = 60_000;
/// 첫 핑으로부터 이 시간이 지나면 중단. 횟수가 아니라 경과 시간으로 끊는다 —
/// 건너뛴 회차가 있으면 횟수 상한은 얼마든지 늘어난다.
pub(crate) const KEEPALIVE_MAX_SPAN_MS: u64 = 8 * 60 * 60 * 1000;
/// 이보다 작은 컨텍스트는 유지할 가치가 없다
pub(crate) const KEEPALIVE_MIN_CTX: u64 = 20_000;
/// Enter 이후 입력창이 비워지고 다시 그려질 때까지 기다리는 시간
pub(crate) const KEEPALIVE_RESTORE_DELAY_MS: u64 = 400;

#[derive(Clone)]
pub(crate) struct KeepAlive {
    pub(crate) enabled: bool,
    /// 남은 TTL이 이보다 적으면 핑
    pub(crate) threshold_secs: u64,
    pub(crate) message: String,
}

pub(crate) static KEEPALIVE: LazyLock<Mutex<KeepAlive>> = LazyLock::new(|| {
    Mutex::new(KeepAlive {
        enabled: false,
        threshold_secs: 120,
        message: "reply \".\" only".into(),
    })
});

#[tauri::command]
pub(crate) fn set_keepalive(enabled: bool, threshold_secs: u64, message: String) {
    let mut k = KEEPALIVE.lock().unwrap_or_else(|e| e.into_inner());
    k.enabled = enabled;
    k.threshold_secs = threshold_secs.clamp(30, 3600);
    if !message.trim().is_empty() {
        k.message = message.trim().to_string();
    }
}

pub(crate) struct Activity {
    /// ＋ 새 세션으로 띄운 탭은 세션 id를 나중에 안다. 알게 되면 여기에 적어 두고
    /// 상태 파일(~/.claude/sessions/<pid>.json)을 그 id로 찾는다 — 없으면 그 세션만
    /// 출력 밀도 추정으로 남아 완료 판정이 둔해진다.
    pub(crate) session_id: Option<String>,
    pub(crate) last_input: Option<std::time::Instant>,
    pub(crate) last_out: Option<std::time::Instant>,
    pub(crate) burst_start: Option<std::time::Instant>,
    pub(crate) working: bool,
    pub(crate) last_check: Option<std::time::Instant>,
    pub(crate) agent: String,
    /// 알림 문구용 탭 제목
    pub(crate) title: String,
    /// 세션 파일 — 턴 종료 판정용. 없으면(새 세션) 침묵 타임아웃만 쓴다.
    pub(crate) file: Option<PathBuf>,
    /// 마지막 제출 이후 뭔가 입력됨 (내용은 몰라도 됨 — 있는지만 알면 된다)
    pub(crate) draft: bool,
    /// 이스케이프 시퀀스 파싱 상태 (0=없음, 1=ESC 직후, 2=CSI 안)
    pub(crate) esc_state: u8,
    /// draft 안의 줄바꿈 수 (Alt+Enter 또는 붙여넣기). Ctrl+U 횟수 계산에 쓴다.
    pub(crate) draft_lines: u32,
    /// 괄호 붙여넣기(ESC[200~ … ESC[201~) 안인지. 붙여넣은 개행은 제출이 아니다.
    pub(crate) in_paste: bool,
    /// CSI 파라미터 앞 3바이트 — 붙여넣기 마커(200/201) 판별용
    pub(crate) csi: [u8; 3],
    pub(crate) csi_len: u8,
    pub(crate) last_ping: Option<std::time::Instant>,
    pub(crate) first_ping: Option<std::time::Instant>,
    /// ~/.claude/sessions의 상태 파일이 이 세션을 보고하고 있는가. 참이면 출력 밀도
    /// 추정은 쓰지 않는다 — 에이전트가 직접 말해 주는데 추측할 이유가 없다.
    pub(crate) file_backed: bool,
    /// 상태 파일이 "입력 필요"라고 보고한 마지막 값
    pub(crate) waiting: bool,
    /// 이 세션을 처음 보고한 프로세스. 나중에 다른 프로세스가 같은 세션을 보고하면
    /// 세션이 그쪽으로 옮겨간 것이고, 우리 터미널은 주인을 잃는다.
    pub(crate) owner_pid: Option<u32>,
    /// 옮겨간 걸 이미 알렸는가 (한 번만 알린다)
    pub(crate) moved: bool,
    /// 옮겨간 후보를 처음 본 시각 — 잠깐 스쳐 가는 값으로 오판하지 않으려고 한 번 더 본다
    pub(crate) moved_seen: Option<(u32, std::time::Instant)>,
}

pub(crate) static ACTIVITY: LazyLock<Mutex<HashMap<String, Activity>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Serialize)]
pub(crate) struct PtyStateEvent {
    pub(crate) id: String,
    pub(crate) working: bool,
    /// 에이전트가 사람의 답을 기다린다 (권한 확인, 질문). 상태 파일이 있을 때만 알 수 있다.
    pub(crate) waiting: bool,
    /// false면 "응답 완료" 알림을 띄우지 않는다 (상태 보정일 뿐 턴 종료가 아닐 때)
    pub(crate) notify: bool,
}

/// 파일 끝부분만 읽는다 (턴 상태는 마지막 레코드에만 있음)
pub(crate) fn read_tail(path: &std::path::Path, limit: u64) -> Option<String> {
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
pub(crate) fn turn_in_progress(file: &std::path::Path) -> bool {
    turn_state(file).unwrap_or(false)
}

/// turn_in_progress와 같지만 "판단할 기록을 못 찾음"을 None으로 돌려준다. 끝부분이 큰
/// 도구 결과 한 줄 안에 걸리면 읽은 조각에 온전한 레코드가 하나도 없다(1MB가 넘는 줄이
/// 실제로 있다). 작업 판정은 그때 "끝남"으로 봐도 침묵 타임아웃이 받쳐 주지만, 캐시
/// 유지 핑은 모르면 보내면 안 된다.
pub(crate) fn turn_state(file: &std::path::Path) -> Option<bool> {
    // Gemini는 턴마다 append하지 않고 통짜 JSON을 다시 쓰므로 신호가 없다.
    // 이런 탭은 침묵 타임아웃에만 의존한다.
    if file.extension().map(|e| e != "jsonl").unwrap_or(true) {
        return Some(false);
    }
    let text = read_tail(file, 32 * 1024)?;
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
                return Some(o["message"]["stop_reason"] == "tool_use");
            }
            "user" => {
                let txt = extract_text(&o["message"]["content"]);
                if txt.trim().starts_with("[Request interrupted") {
                    return Some(false); // 사용자가 중단함
                }
                // 프롬프트 제출 또는 tool_result → 에이전트 차례
                return Some(true);
            }
            // codex는 턴의 시작과 끝을 직접 기록한다 — 화면 출력을 눈치로 읽을 필요가 없다.
            // task_complete / turn_aborted 뒤에 token_count가 더 붙으므로 그건 건너뛴다.
            "event_msg" => match o["payload"]["type"].as_str().unwrap_or("") {
                "task_started" => return Some(true),
                "task_complete" | "turn_aborted" | "error" => return Some(false),
                // 예전 rollout에는 task_* 이벤트가 없어 메시지로 판단한다
                "agent_message" => return Some(false),
                "user_message" => return Some(true),
                _ => continue,
            },
            _ => continue,
        }
    }
    None
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
pub(crate) fn send_keepalive(app: AppHandle, id: String, agent: String, message: String, draft: bool) {
    // 세 단계를 한 번에 쓰기 스레드에 넘긴다. 순서와 사이의 기다림은 그 스레드가 지킨다.
    // 스페이스 → Ctrl+U 로 한 줄을 kill ring에 옮긴다. 스페이스를 먼저 넣는 건 입력이
    // 비어 있어도 kill ring에 이번 내용이 확실히 들어가게 하고, 빈 프롬프트에서 Ctrl+U가
    // 다른 동작을 하는 것도 막기 위해서다. 여러 줄 draft는 애초에 보내지 않으므로
    // (keepalive_candidates) 한 번이면 충분하다.
    let state = app.state::<PtyState>();
    let map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    let Some(p) = map.get(&id) else { return }; // 탭이 닫혔다
    let jobs = [
        PtyInput::Bytes(vec![b' ', 0x15]),
        PtyInput::Bytes(format!("{}\r", message).into_bytes()),
        PtyInput::Pause(std::time::Duration::from_millis(KEEPALIVE_RESTORE_DELAY_MS)),
        PtyInput::Bytes(vec![0x19, 0x7f]),
    ];
    let ok = jobs.into_iter().all(|j| p.input.send(j).is_ok());
    drop(map);
    trace(&id, &agent, "keepalive", &format!("{} (draft={}{})", message, draft, if ok { "" } else { ", 실패" }));
}

/// 남은 캐시 TTL(초)과 티어. 세션 파일에서 직접 읽으므로 프런트 폴링 상태와 무관하다.
pub(crate) fn cache_ttl_remaining(file: &PathBuf) -> Option<(f64, u32)> {
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

/// 이 쓰기가 사람이 친 것인가. 터미널은 사람 몰래도 많이 보낸다 — 마우스 신호(풀스크린
/// 클로드는 스치기만 해도 받는다), 포커스 알림, 질의에 대한 자동 응답. 이것까지 "방금
/// 입력함"으로 치면, 마우스를 올려 둔 탭의 출력이 전부 타이핑 에코로 빠져 작업 판정이
/// 멎고, 캐시 유지도 "방금 타이핑했다"며 계속 건너뛴다. 실측에서 보낸 입력의 88%가
/// 마우스 신호였다.
/// 섞여 있으면(자동 응답 + 글자) 사람의 입력으로 본다.
pub(crate) fn is_user_input(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            // CSI: 파라미터·중간 바이트 뒤 종료 바이트(0x40~0x7e)
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            let Some(&fin) = bytes.get(j) else { return true };
            let params = &bytes[start..j];
            let auto = match fin {
                // SGR 마우스 (ESC[<b;x;yM / m)
                b'M' | b'm' => params.first() == Some(&b'<'),
                // 포커스 들어옴/나감 (ESC[I / ESC[O)
                b'I' | b'O' => params.is_empty(),
                // 장치 속성 응답 (ESC[?..c / ESC[>..c), 커서 위치 보고 (ESC[r;cR)
                b'c' => matches!(params.first(), Some(b'?') | Some(b'>')),
                b'R' => !params.is_empty() && params.contains(&b';'),
                // 모드 보고 (ESC[?..$y)
                b'y' => params.last() == Some(&b'$'),
                _ => false,
            };
            if !auto {
                return true;
            }
            i = j + 1;
        } else {
            return true;
        }
    }
    false
}

/// 사용자가 입력창에 뭔가 써 뒀는지 추적한다. 내용은 알 필요 없고 있는지만 알면 된다.
///
/// 터미널은 앱의 질의에 이스케이프 시퀀스로 **자동 응답**한다(커서 위치 보고 등).
/// 그 응답에도 숫자·문자가 들어 있어서 단순히 "출력 가능 문자 = 타이핑"으로 세면
/// 자리를 비운 사이에도 draft가 참으로 굳는다 — 실측에서 85분 동안 427건이 들어왔다.
/// 그래서 시퀀스를 건너뛴 뒤에 남는 문자만 입력으로 인정한다.
pub(crate) fn note_draft(a: &mut Activity, bytes: &[u8]) {
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
/// 임시 id로 뜬 탭이 어떤 세션이 됐는지 알려준다. 상태 파일과 기록 파일을 그때부터
/// 그 세션 것으로 읽는다.
#[tauri::command(async)]
pub(crate) fn bind_session(id: String, session_id: String, file: String, title: Option<String>) {
    let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(a) = act.get_mut(&id) else { return };
    a.session_id = Some(session_id);
    // 새 세션 탭은 "프로젝트 · 새 세션"으로 떠 있다가 이름을 알게 된다 — 알림도 그 이름으로
    if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
        a.title = t;
    }
    let f = file.trim();
    if !f.is_empty() {
        a.file = Some(PathBuf::from(f));
    }
}

pub(crate) fn note_output(id: &str) {
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

/// Claude Code(2.1.2xx+)가 프로세스마다 쓰는 상태 파일 `~/.claude/sessions/<pid>.json`.
/// status 값: busy/working/compacting/shell = 작업 중, blocked/waiting(또는 waitingFor가 있음) =
/// 사람의 답을 기다림, idle/exited = 대기. 세션 ID → (작업 중, 입력 필요). 프로세스가 끝나면 파일도 지워지지만, 비정상 종료로 남은
/// 파일이 영원히 "작업 중"으로 읽히지 않게 갱신 시각이 오래된 건 버린다.
pub(crate) fn scan_session_status() -> (HashMap<String, (bool, bool)>, HashMap<String, u32>) {
    let mut out = HashMap::new();
    let mut owner = HashMap::new();
    let mut latest: HashMap<String, f64> = HashMap::new();
    let mut owner_latest: HashMap<String, f64> = HashMap::new();
    let Some(home) = dirs::home_dir() else { return (out, owner) };
    let Ok(entries) = fs::read_dir(home.join(".claude").join("sessions")) else {
        return (out, owner);
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "json").unwrap_or(true) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(sid) = v["sessionId"].as_str() else { continue };
        // 주인 판정은 status가 없는 파일도 센다. 세션을 이어받은 프로세스가 status 없이
        // 파일만 올려 둔 채 굳어 있는 것을 실제로 봤다(updatedAt이 80분 전에 멈춤).
        // 그 파일을 건너뛰면 정작 잡아야 할 인수인계를 통째로 놓친다.
        let pid = v["pid"].as_u64().unwrap_or(0) as u32;
        let stamp = v["updatedAt"].as_f64().or_else(|| v["startedAt"].as_f64()).unwrap_or(0.0);
        // 강제로 끝난 프로세스는 파일을 못 지운다. 그 파일을 믿으면 새로 연 탭이 죽은
        // 프로세스의 "작업 중"을 물려받고, 주인이 바뀐 것처럼 보인다.
        let proc_start = v["procStart"].as_str().and_then(|s| s.parse::<u64>().ok());
        if pid != 0 && !process_is(pid, proc_start) {
            continue;
        }
        if pid != 0 && now_ms - stamp <= 6.0 * 3600.0 * 1000.0 {
            match owner_latest.get(sid) {
                Some(&t) if t >= stamp => {}
                _ => {
                    owner_latest.insert(sid.to_string(), stamp);
                    owner.insert(sid.to_string(), pid);
                }
            }
        }
        let Some(status) = v["status"].as_str() else { continue };
        let updated = v["updatedAt"].as_f64().unwrap_or(0.0);
        if now_ms - updated > 6.0 * 3600.0 * 1000.0 {
            continue;
        }
        let working = matches!(status, "busy" | "working" | "compacting" | "shell");
        let waiting = matches!(status, "blocked" | "waiting")
            || v["waitingFor"].as_str().map(|w| !w.is_empty()).unwrap_or(false);
        // 같은 세션을 두 프로세스가 보고할 수 있다(데몬 워커 + attach 클라이언트, 이중
        // --resume). 디렉터리 순서에 맡기지 않고 가장 최근에 갱신된 쪽을 믿는다.
        match latest.get(sid) {
            Some(&t) if t >= updated => {}
            _ => {
                latest.insert(sid.to_string(), updated);
                out.insert(sid.to_string(), (working, waiting));
            }
        }
    }
    (out, owner)
}

/// 세션이 우리가 띄운 프로세스에서 다른 프로세스로 넘어갔다. 그 탭의 터미널에는
/// 화면을 그리고 키를 읽던 쪽이 더는 없다 — 사용자가 알아야 고칠 수 있다.
#[derive(Clone, Serialize)]
pub(crate) struct SessionMovedEvent {
    pub(crate) id: String,
    pub(crate) from: u32,
    pub(crate) to: u32,
}

/// 이 세션의 주인이 바뀌었는지 본다. 한 번 스쳐 본 값으로는 판정하지 않는다 —
/// 세션을 넘기는 동안 두 프로세스의 파일이 잠깐 같이 있을 수 있어서, 같은 새 주인이
/// 이 시간을 넘겨 유지될 때만 알린다.
const OWNER_CONFIRM_MS: u64 = 5_000;

pub(crate) fn check_owner(
    id: &str,
    a: &mut Activity,
    pid: u32,
    now: std::time::Instant,
    moved: &mut Vec<(String, u32, u32)>,
) {
    let Some(mine) = a.owner_pid else {
        a.owner_pid = Some(pid);
        return;
    };
    if pid == mine {
        a.moved_seen = None;
        // 돌아왔으면 경고도 거둬야 한다. 다른 터미널에서 잠깐 이어받았다가 닫은
        // 경우까지 영영 "세션을 잃었다"로 남겨 두면, 멀쩡한 탭을 다시 열게 만든다.
        if a.moved {
            a.moved = false;
            trace(id, &a.agent, "state", &format!("session back {pid}"));
            moved.push((id.to_string(), pid, 0));
        }
        return;
    }
    if a.moved {
        return;
    }
    match a.moved_seen {
        Some((seen, t)) if seen == pid => {
            if now.duration_since(t).as_millis() as u64 >= OWNER_CONFIRM_MS {
                a.moved = true;
                trace(id, &a.agent, "state", &format!("session moved {mine}->{pid}"));
                moved.push((id.to_string(), mine, pid));
            }
        }
        _ => a.moved_seen = Some((pid, now)),
    }
}

/// 버스트 상태를 주기적으로 평가해 working 전이를 이벤트로 올린다.
/// JS 타이머가 아니라 여기서 판정하는 게 요점 — 창이 백그라운드로 가도 멈추지 않는다.
/// 상태 파일이 있는 세션은 그 값을 그대로 쓰고, 없는 세션(codex/gemini, 파일이 아직
/// 안 생긴 새 세션)만 출력 밀도로 추정한다.
pub(crate) fn spawn_state_monitor(app: AppHandle) {
    std::thread::spawn(move || {
        let mut tick: u64 = 0;
        let mut file_status: HashMap<String, (bool, bool)> = HashMap::new();
        let mut session_owner: HashMap<String, u32> = HashMap::new();
        loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        tick = tick.wrapping_add(1);
        let now = std::time::Instant::now();
        let ms = |a: std::time::Instant, b: std::time::Instant| b.duration_since(a).as_millis() as u64;

        // 캐시 유지 검사 — 30초에 한 번이면 2분 임계에 충분하다
        if tick % 120 == 0 {
            keepalive_pass(&app, now);
        }
        // 상태 파일은 1초에 한 번 (파일 몇 개를 읽는 정도라 부담이 없다)
        if tick % 4 == 0 {
            (file_status, session_owner) = scan_session_status();
        }

        // 1단계: 잠금 안에서 판정에 필요한 것만 모은다 (파일 I/O는 잠금 밖에서)
        let mut turn_on: Vec<String> = Vec::new();
        let mut quiet_off: Vec<String> = Vec::new();
        let mut candidates: Vec<(String, Option<PathBuf>, u64)> = Vec::new();
        let mut file_off: Vec<String> = Vec::new();
        let mut wait_changed: Vec<(String, bool)> = Vec::new();
        let mut moved: Vec<(String, u32, u32)> = Vec::new();
        {
            let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
            for (id, a) in act.iter_mut() {
                let owner = {
                    let key = a.session_id.as_deref().unwrap_or(id.as_str());
                    session_owner.get(key).copied()
                };
                if let Some(pid) = owner {
                    check_owner(id, a, pid, now, &mut moved);
                }
                let key = a.session_id.as_deref().unwrap_or(id.as_str());
                if let Some(&(w, wt)) = file_status.get(key) {
                    let first = !a.file_backed;
                    a.file_backed = true;
                    if wt != a.waiting {
                        a.waiting = wt;
                        trace(id, &a.agent, "state", if wt { "waiting(file)" } else { "unwait(file)" });
                        wait_changed.push((id.clone(), wt));
                    }
                    if w && !a.working {
                        a.working = true;
                        trace(id, &a.agent, "state", "working(file)");
                        turn_on.push(id.clone());
                    } else if !w && a.working {
                        a.working = false;
                        trace(id, &a.agent, "state", "idle(file)");
                        // 처음 파일을 본 순간의 idle은 시작 화면 출력을 버스트로 오인한
                        // 것을 바로잡는 것이지 턴 종료가 아니다 — 알림 없이 상태만 되돌린다
                        if first { quiet_off.push(id.clone()) } else { file_off.push(id.clone()) }
                    }
                    continue;
                }
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
        turn_off.extend(file_off);
        for (id, from, to) in moved {
            let _ = app.emit("session-moved", SessionMovedEvent { id, from, to });
        }
        for id in turn_on {
            let _ = app.emit("pty-state", PtyStateEvent { id, working: true, waiting: false, notify: true });
        }
        for id in quiet_off {
            let _ = app.emit("pty-state", PtyStateEvent { id, working: false, waiting: false, notify: false });
        }
        // 입력 필요 전이는 작업 중/대기와 독립이다. 프런트가 waiting만 갱신하도록 working은
        // 현재 값을 그대로 싣는다.
        for (id, wt) in wait_changed {
            let working = ACTIVITY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .map(|a| a.working)
                .unwrap_or(false);
            if wt {
                // 완료 알림과 같은 이유로 여기서 직접 보낸다 (창이 백그라운드면 JS가 늦다)
                let title = ACTIVITY
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id)
                    .map(|a| a.title.clone())
                    .unwrap_or_default();
                let _ = app
                    .notification()
                    .builder()
                    .title("✋ 입력 필요")
                    .body(if title.is_empty() { "세션" } else { &title })
                    .show();
            }
            let _ = app.emit("pty-state", PtyStateEvent { id, working, waiting: wt, notify: false });
        }
        for id in turn_off {
            // 창이 최소화·백그라운드면 WebView2가 렌더러를 재워서 JS 리스너가 돌지
            // 않는다 — 알림이 가장 필요한 순간이 정확히 그때이므로 여기서 직접 보낸다.
            // 포커스가 있을 때는 JS가 앱 내 토스트를 띄우므로 중복되지 않는다.
            let focused = app
                .get_webview_window("main")
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false);
            let (agent, title, waiting, ping_turn) = {
                let act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
                let a = act.get(&id);
                (
                    a.map(|a| a.agent.clone()).unwrap_or_default(),
                    a.map(|a| a.title.clone()).unwrap_or_default(),
                    a.map(|a| a.waiting).unwrap_or(false),
                    a.map(ping_turn).unwrap_or(false),
                )
            };
            // 작업이 멈춘 이유가 "사람의 답을 기다림"이면 끝난 게 아니다. 이미 "입력 필요"를
            // 알렸고, 여기서 waiting:false를 실어 보내면 그 표시까지 지워 버린다.
            // 캐시 유지 핑이 일으킨 턴도 사용자에게 알릴 일이 아니다(8시간 동안 매번 뜬다).
            if waiting || ping_turn {
                trace(&id, &agent, "notify", if waiting { "skip:waiting" } else { "skip:ping" });
                let _ = app.emit("pty-state", PtyStateEvent { id, working: false, waiting, notify: false });
                continue;
            }
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
            let _ = app.emit("pty-state", PtyStateEvent { id, working: false, waiting: false, notify: true });
        }
        }
    });
}

/// 방금 끝난 턴이 캐시 유지 핑으로 시작된 것인가. 핑은 write_pty를 거치지 않으므로
/// last_input을 건드리지 않는다 — 핑 이후에 사람이 친 게 없으면 핑의 턴이다.
const PING_TURN_WINDOW_MS: u128 = 10 * 60 * 1000;
pub(crate) fn ping_turn(a: &Activity) -> bool {
    let Some(p) = a.last_ping else { return false };
    if p.elapsed().as_millis() > PING_TURN_WINDOW_MS {
        return false;
    }
    a.last_input.map(|i| i < p).unwrap_or(true)
}

/// 핑을 보낼 수 있는 탭 — 잠금 안에서는 여기까지만 고른다 (파일 파싱은 잠금 밖에서)
pub(crate) fn keepalive_candidates() -> Vec<(String, String, PathBuf, bool, u32)> {
    let now = std::time::Instant::now();
    let ms = |a: std::time::Instant| now.duration_since(a).as_millis() as u64;
    let mut candidates: Vec<(String, String, PathBuf, bool, u32)> = Vec::new();
    {
        let act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
        for (id, a) in act.iter() {
            let Some(file) = a.file.clone() else { continue };
            if a.working {
                continue; // 작업 중이면 애초에 캐시가 살아 있다
            }
            // 권한 확인·질문 창이 떠 있으면 절대 보내지 않는다. 핑 끝의 Enter가 떠 있는
            // 선택지를 그대로 고른다 — 자리를 비운 사이 도구 실행을 승인해 버린다.
            if a.waiting {
                continue;
            }
            // 쓰다 만 한 줄은 보낸다 — 전송 시퀀스가 Ctrl+U로 kill ring에 옮겼다가
            // Ctrl+Y로 되돌린다. 여러 줄은 Ctrl+U가 마지막 줄만 치우므로 앞줄이 핑과 함께
            // 실제 질문으로 나간다. 그래서 건너뛴다.
            if a.draft_lines > 0 {
                continue;
            }
            if a.last_input.map(|t| ms(t) < KEEPALIVE_INPUT_QUIET_MS).unwrap_or(false) {
                continue; // 방금 타이핑했다
            }
            if a.first_ping.map(|t| ms(t) > KEEPALIVE_MAX_SPAN_MS).unwrap_or(false) {
                continue; // 너무 오래 자리를 비웠다 — 그만 유지한다
            }
            candidates.push((id.clone(), a.agent.clone(), file, a.draft, a.draft_lines));
        }
    }
    candidates
}

/// 만료가 임박한 세션에 캐시 유지 핑을 보낸다.
pub(crate) fn keepalive_pass(app: &AppHandle, now: std::time::Instant) {
    let cfg = KEEPALIVE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !cfg.enabled {
        return;
    }
    let ms = |a: std::time::Instant| now.duration_since(a).as_millis() as u64;
    let candidates = keepalive_candidates();

    for (id, agent, file, draft, _lines) in candidates {
        // 상태 파일이 없는 에이전트(codex·gemini)나 상태를 늦게 쓰는 경우를 위한 두 번째
        // 확인: 기록상 턴이 아직 안 끝났으면(도구 결과·권한을 기다림) 보내지 않는다.
        if turn_state(&file) != Some(false) {
            continue;
        }
        let Some((remain, ttl)) = cache_ttl_remaining(&file) else { continue };
        if remain <= 0.0 || remain > cfg.threshold_secs as f64 {
            continue; // 이미 만료됐거나 아직 여유 있음
        }
        let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
        let Some(a) = act.get_mut(&id) else { continue };
        // 후보를 고른 뒤 파일을 읽는 사이에 권한 창이 떴을 수 있다 — 보내기 직전에 다시 본다
        if a.waiting || a.working || a.draft_lines > 0 {
            continue;
        }
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

#[cfg(test)]
mod tests {
    /// codex는 턴의 시작·끝을 이벤트로 남긴다. token_count가 뒤에 더 붙어도
    /// 끝난 턴을 진행 중으로 보면 탭이 영영 작업중으로 남는다.
    /// 임시 id로 뜬 탭이 세션을 알게 되면, 상태 파일을 그 세션 id로 찾아야 한다.
    #[test]
    fn a_bound_tab_is_looked_up_by_its_session() {
        let mut act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
        act.insert("new-1".to_string(), Activity { agent: "claude".into(), ..blank_activity() });
        drop(act);

        bind_session("new-1".into(), "sess-9".into(), "C:/tmp/sess-9.jsonl".into(), None);

        let act = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
        let a = act.get("new-1").expect("항목이 남아 있어야 한다");
        assert_eq!(a.session_id.as_deref(), Some("sess-9"));
        assert_eq!(a.file.as_deref(), Some(std::path::Path::new("C:/tmp/sess-9.jsonl")));
        // 감시 루프가 쓰는 열쇠
        let key = a.session_id.as_deref().unwrap_or("new-1");
        assert_eq!(key, "sess-9", "상태 파일은 세션 id로 찾는다");
    }

    #[test]
    fn codex_task_events_decide_the_turn() {
        let dir = std::env::temp_dir().join(format!("deck-codex-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("rollout.jsonl");
        let started = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let done = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;
        let tok = r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#;

        std::fs::write(&f, format!("{started}
{tok}
")).unwrap();
        assert!(super::turn_in_progress(&f), "task_started 뒤면 작업 중");

        std::fs::write(&f, format!("{started}
{done}
{tok}
")).unwrap();
        assert!(!super::turn_in_progress(&f), "task_complete 뒤의 token_count는 무시");

        let aborted = r#"{"type":"event_msg","payload":{"type":"turn_aborted"}}"#;
        std::fs::write(&f, format!("{started}
{aborted}
")).unwrap();
        assert!(!super::turn_in_progress(&f), "중단된 턴은 작업 중이 아니다");
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

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

    /// 이 기기의 상태 파일로 실제로 본다. `cargo test -- --ignored real_session_owners --nocapture`
    #[test]
    #[ignore]
    fn real_session_owners() {
        let (status, owner) = scan_session_status();
        eprintln!("살아 있는 주인 {:?}", owner);
        eprintln!("상태 {:?}", status);
    }

    /// 사람이 친 것과 터미널이 알아서 보낸 것을 가른다. 실제 기록에 나온 모양들이다.
    #[test]
    fn auto_reports_are_not_user_input() {
        // 마우스 움직임·휠, 포커스, 장치 속성 응답, 커서 위치 보고, 여러 개가 한 번에
        for s in [
            "\x1b[<35;110;6M", "\x1b[<64;10;5M", "\x1b[<0;3;4m", "\x1b[I", "\x1b[O",
            "\x1b[?1;2c", "\x1b[>0;276;0c", "\x1b[12;40R", "\x1b[?2026;2$y",
            "\x1b[<35;1;1M\x1b[<35;2;1M",
        ] {
            assert!(!is_user_input(s.as_bytes()), "{s:?}");
        }
        // 글자, 한글, 엔터, 방향키, Ctrl+C, 붙여넣기, 자동 응답 뒤에 섞인 글자
        for s in ["a", "한", "\r", "\x1b[A", "\x03", "\x1b[200~hi\x1b[201~", "\x1b[Ix", "\x1b"] {
            assert!(is_user_input(s.as_bytes()), "{s:?}");
        }
    }

    /// 권한 확인 창이 떠 있는 세션에는 핑을 보내지 않는다. 끝의 Enter가 떠 있는 선택지를
    /// 고르면 사람 없이 도구 실행이 승인된다.
    #[test]
    fn keepalive_never_targets_a_waiting_or_multiline_session() {
        let dir = std::env::temp_dir().join(format!("deck-ka-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        std::fs::write(&file, "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n").unwrap();
        let id = format!("ka-test-{}", std::process::id());
        let picked = |a: Activity| {
            ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), a);
            let c = keepalive_candidates();
            ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            c.iter().any(|x| x.0 == id)
        };
        let base = || Activity { agent: "claude".into(), file: Some(file.clone()), ..blank_activity() };
        assert!(picked(base()), "대기 중이 아닌 한가한 세션은 후보다");
        assert!(!picked(Activity { waiting: true, ..base() }), "권한 창");
        assert!(!picked(Activity { draft_lines: 2, draft: true, ..base() }), "여러 줄 초안");
        // 기록상 도구 결과를 기다리는 턴이면 상태 파일이 없어도 보내지 않는다
        std::fs::write(&file, "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\"}}\n").unwrap();
        assert_eq!(turn_state(&file), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 세션이 다른 프로세스로 넘어간 것과, 넘기는 도중 잠깐 두 파일이 겹친 것을
    /// 가른다. 잠깐 보인 값으로 "터미널을 잃었다"고 알리면 멀쩡한 탭에 경고가 뜬다.
    #[test]
    fn owner_change_is_reported_once_and_only_after_it_holds() {
        let mut a = blank_activity();
        let t0 = std::time::Instant::now();
        let mut moved = Vec::new();

        // 처음 본 프로세스가 주인이다
        check_owner("tab", &mut a, 100, t0, &mut moved);
        assert_eq!(a.owner_pid, Some(100));
        assert!(moved.is_empty());

        // 다른 프로세스를 한 번 봤다고 바로 알리지 않는다
        check_owner("tab", &mut a, 200, t0, &mut moved);
        assert!(moved.is_empty());
        // 원래 주인이 다시 보이면 없던 일이 된다
        check_owner("tab", &mut a, 100, t0 + std::time::Duration::from_secs(1), &mut moved);
        assert!(moved.is_empty());
        assert!(a.moved_seen.is_none());

        // 같은 새 주인이 확인 시간을 넘겨 유지되면 알린다 — 한 번만
        check_owner("tab", &mut a, 200, t0 + std::time::Duration::from_secs(2), &mut moved);
        check_owner("tab", &mut a, 200, t0 + std::time::Duration::from_secs(8), &mut moved);
        assert_eq!(moved, vec![("tab".to_string(), 100, 200)]);
        check_owner("tab", &mut a, 200, t0 + std::time::Duration::from_secs(9), &mut moved);
        assert_eq!(moved.len(), 1);

        // 주인이 우리 쪽으로 돌아오면 경고를 거둔다(to=0). 다른 터미널에서 잠깐
        // 열었다 닫은 경우까지 영영 경고로 남기지 않는다.
        check_owner("tab", &mut a, 100, t0 + std::time::Duration::from_secs(10), &mut moved);
        assert!(!a.moved);
        assert_eq!(moved.last(), Some(&("tab".to_string(), 100, 0)));
    }

    fn blank_activity() -> Activity {
        Activity {
            session_id: None,
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
            file_backed: false,
            waiting: false,
            owner_pid: None,
            moved: false,
            moved_seen: None,
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
}
