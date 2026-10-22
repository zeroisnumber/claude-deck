// PTY 생성·입출력·종료

use super::*;
use std::sync::{Arc, Condvar};

// ---------- PTY 관리 ----------

pub(crate) struct PtyInstance {
    pub(crate) master: Box<dyn MasterPty + Send>,
    /// 입력은 탭마다 따로 도는 쓰기 스레드로 보낸다. 예전에는 전 탭이 같이 쓰는 잠금을
    /// 쥔 채 메인 스레드에서 PTY에 직접 썼다 — 한 탭의 입력 통로가 막히면(에이전트가
    /// 입력을 안 읽는 동안 큰 붙여넣기) 창 전체와 다른 탭 입력이 같이 멈췄다.
    /// 스레드 하나가 받은 순서대로 쓰므로 키 순서는 그대로다.
    pub(crate) input: std::sync::mpsc::Sender<PtyInput>,
    pub(crate) killer: Box<dyn ChildKiller + Send + Sync>,
    /// 트레이스 라벨 (claude/codex/gemini) — write_pty에서 입력 이벤트를 찍을 때 씀
    pub(crate) agent: String,
    /// 같은 id로 재시작(kill 직후 spawn)했을 때, 죽은 이전 프로세스의 대기 스레드가
    /// 새 인스턴스를 지워버리지 않도록 구분하는 세대 번호
    pub(crate) generation: u64,
}

pub(crate) static PTY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 쓰기 스레드가 받는 일. 기다림도 쓰기 스레드가 한다 — 보내는 쪽에서 재면 "넘긴
/// 시각"부터 세게 되어, 쓰기가 밀렸을 때 캐시 유지 핑의 세 단계(치우기·보내기·되돌리기)가
/// 한꺼번에 들어가 되돌리기가 입력창이 비기 전에 도착한다.
pub(crate) enum PtyInput {
    Bytes(Vec<u8>),
    Pause(std::time::Duration),
}

/// 프로세스 종료 처리 — 이 세대가 아직 유효할 때만 맵에서 지우고 이벤트를 보낸다.
/// (kill_pty로 이미 정리됐거나 재시작된 경우에는 아무것도 하지 않음)
pub(crate) fn finish_pty(app: &AppHandle, id: &str, generation: u64) {
    let state = app.state::<PtyState>();
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    match map.get(id) {
        Some(p) if p.generation == generation => {}
        _ => return,
    }
    map.remove(id);
    // 맵 잠금 안에서 같이 지운다 — 놓은 뒤에 지우면 그 사이 같은 id로 새로 띄운 탭의
    // 상태 항목을 지울 수 있다 (kill_pty와 같은 순서)
    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    drop(map);
    let _ = app.emit("pty-exit", PtyExit { id: id.to_string() });
}

#[derive(Default)]
pub(crate) struct PtyState(pub(crate) Mutex<HashMap<String, PtyInstance>>);

#[derive(Clone, Serialize)]
pub(crate) struct PtyOutput {
    pub(crate) id: String,
    pub(crate) data: String, // base64
    /// 어느 실행에서 나온 출력인가. 탭을 다시 열면 같은 id로 새 프로세스가 뜨는데,
    /// 끈 프로세스의 리더 스레드가 남은 출력을 마저 흘려서 새 화면에 섞였다.
    pub(crate) generation: u64,
    /// 진단 기록이 켜져 있을 때만: 보낸 벽시계 시각(ms). 화면 쪽이 받은 시각과 빼서 이벤트가
    /// 메인 스레드·WebView2를 지나는 동안 얼마나 밀렸는지 본다 — 화면 쪽 계측엔 안 보이는 구간이다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sent: Option<f64>,
}

/// 벽시계 ms(소수점까지). 화면 쪽의 Date.now()와 같은 시계다.
pub(crate) fn wall_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// 입력이 무엇인가 — 클로드가 마우스를 받는 동안엔 커서만 움직여도 SGR 보고가 나간다.
/// 기록에서 휠(w)·움직임(m)·누름(c)과 그 밖(k)을 가르려고.
pub(crate) fn input_kind(data: &[u8]) -> char {
    let Some(rest) = data.strip_prefix(b"\x1b[<") else { return 'k' };
    let code: u32 = rest
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .fold(0, |n, b| n * 10 + (b - b'0') as u32);
    if code & 64 != 0 { 'w' } else if code & 32 != 0 { 'm' } else { 'c' }
}

#[derive(Clone, Serialize)]
pub(crate) struct PtyExit {
    pub(crate) id: String,
}

/// 몰려오는 출력을 묶는 버퍼. 마지막 전송에서 FLUSH_MS가 지났고 모아둔 것이 없으면
/// 곧바로 보내고(에코 지연 없음), 그 사이에 오는 조각은 타이머가 한 번에 비운다.
const FLUSH_MS: u64 = 8;
const FLUSH_MAX: usize = 256 * 1024;

pub(crate) struct OutBuf {
    pending: Vec<u8>,
    last_emit: Option<std::time::Instant>,
}

impl OutBuf {
    pub(crate) fn new() -> Self {
        OutBuf { pending: Vec::new(), last_emit: None }
    }

    /// 읽은 조각을 넣는다. 지금 보내야 하면 보낼 바이트를 돌려준다.
    pub(crate) fn push(&mut self, now: std::time::Instant, data: &[u8]) -> Option<Vec<u8>> {
        let quiet = self
            .last_emit
            .map(|t| now.duration_since(t).as_millis() as u64 >= FLUSH_MS)
            .unwrap_or(true);
        if self.pending.is_empty() && quiet {
            self.last_emit = Some(now);
            return Some(data.to_vec());
        }
        self.pending.extend_from_slice(data);
        // 한 번에 너무 크게 불어나면 타이머를 기다리지 않는다
        if self.pending.len() >= FLUSH_MAX {
            self.last_emit = Some(now);
            return Some(std::mem::take(&mut self.pending));
        }
        None
    }

    /// 타이머가 부르는 배출 — 모아둔 게 있을 때만 준다.
    pub(crate) fn take(&mut self, now: std::time::Instant) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            return None;
        }
        self.last_emit = Some(now);
        Some(std::mem::take(&mut self.pending))
    }

    /// 스레드가 끝날 때 남은 것을 비운다.
    pub(crate) fn drain(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() { None } else { Some(std::mem::take(&mut self.pending)) }
    }
}

/// 보내는 일을 잠금 안에서 한다. 잠금을 놓고 보내면, 배출 스레드가 그 사이에 밀렸을 때
/// 읽기 스레드가 "조용하다"고 판단해 다음 조각을 먼저 보내 화면 글자가 뒤섞인다.
pub(crate) struct OutPipe {
    buf: Mutex<OutBuf>,
    cv: Condvar,
}

impl OutPipe {
    pub(crate) fn new() -> Self {
        OutPipe { buf: Mutex::new(OutBuf::new()), cv: Condvar::new() }
    }

    pub(crate) fn write(&self, now: std::time::Instant, data: &[u8], emit: &dyn Fn(&[u8])) {
        let mut b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        match b.push(now, data) {
            Some(chunk) => emit(&chunk),
            None => self.cv.notify_one(),
        }
    }

    pub(crate) fn flush(&self, now: std::time::Instant, emit: &dyn Fn(&[u8])) {
        let mut b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(chunk) = b.take(now) {
            emit(&chunk);
        }
    }

    pub(crate) fn drain(&self, emit: &dyn Fn(&[u8])) {
        let mut b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(chunk) = b.drain() {
            emit(&chunk);
        }
    }

    /// 모인 게 생길 때까지 잔다. 깨어난 이유가 데이터면 true.
    pub(crate) fn wait_for_data(&self, timeout: std::time::Duration) -> bool {
        let b = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if !b.pending.is_empty() {
            return true;
        }
        let (b, _) = self.cv.wait_timeout(b, timeout).unwrap_or_else(|e| e.into_inner());
        !b.pending.is_empty()
    }

    /// 끝낼 때 자고 있는 스레드를 깨운다.
    pub(crate) fn wake(&self) {
        self.cv.notify_all();
    }

    /// 보내는 동안 잠금을 쥐고 있는지 확인하는 시험용 창구.
    #[cfg(test)]
    fn locked_now(&self) -> bool {
        self.buf.try_lock().is_err()
    }
}

/// 어느 ConPTY가 붙었나. portable-pty는 첫 PTY를 열 때 exe 옆의 conpty.dll(conpty/README.md)을
/// 먼저 찾고, 없으면 윈도우 내장(kernel32)을 쓴다. 불러온 conpty.dll이 있으면 그 경로를 준다.
fn conpty_in_use() -> String {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    let name: Vec<u16> = "conpty.dll\0".encode_utf16().collect();
    unsafe {
        let module = GetModuleHandleW(name.as_ptr());
        if module.is_null() {
            return "inbox".to_string();
        }
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

fn emit_output(app: &AppHandle, id: &str, generation: u64, data: &[u8]) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let sent = PTY_TRACE.load(Ordering::Relaxed).then(wall_ms);
    let _ = app.emit("pty-output", PtyOutput { id: id.to_string(), data: encoded, generation, sent });
}

/// 새 세션 창에서 시작 전에 폴더가 있는지 본다. 네트워크 드라이브면 stat도 멈출 수
/// 있어 메인 스레드에 두지 않는다.
#[tauri::command(async)]
pub(crate) fn dir_exists(path: String) -> bool {
    // "game"처럼 상대 경로면 앱 프로세스의 현재 폴더 기준으로 보게 된다 — 뜻한 곳이 아니다
    let p = PathBuf::from(path);
    p.is_absolute() && p.is_dir()
}

/// 없는 폴더를 사용자가 만들겠다고 했을 때만 부른다. 새 프로젝트를 여는 게 새 세션의
/// 흔한 용도라, 창을 닫고 탐색기에서 만들어 오게 하지 않는다.
#[tauri::command(async)]
pub(crate) fn create_dir(path: String) -> Result<(), String> {
    if !PathBuf::from(&path).is_absolute() {
        return Err("C:\\로 시작하는 전체 경로를 적어 주세요".into());
    }
    std::fs::create_dir_all(&path).map_err(|e| format!("폴더를 만들 수 없습니다: {e}"))
}

/// 이 프로세스가 언제 시작됐는가 (윈도우 FILETIME). 클로드가 상태 파일에 적어 두는
/// procStart와 같은 값이라, 번호가 재사용된 남의 프로세스를 가려낼 수 있다.
fn process_start(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let mut created = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let ok = unsafe {
        GetProcessTimes(handle, &mut created, &mut zero, &mut zero.clone(), &mut zero.clone())
    };
    if ok == 0 {
        return None;
    }
    Some(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
}

/// 이 번호의 프로세스가 지금 살아 있고, 상태 파일이 적어 둔 그 프로세스가 맞는가.
/// 우리가 탭을 끌 때는 클로드를 강제로 끝내므로 클로드가 자기 상태 파일을 못 지운다.
/// 그 파일을 주인으로 믿으면 다시 연 탭에 "세션이 옮겨갔습니다"가 거짓으로 뜨고,
/// 그 띠의 종료 버튼이 우리 쪽 클로드를 끈다.
pub(crate) fn process_is(pid: u32, proc_start: Option<u64>) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let started = process_start(h);
        CloseHandle(h);
        match (proc_start, started) {
            (Some(want), Some(got)) => want == got,
            // 파일에 시작 시각이 없으면(옛 판) 살아 있는 것만 본다
            (None, Some(_)) => true,
            _ => false,
        }
    }
}

/// 지금 떠 있는 프로세스 전부의 부모 번호 (자식 → 부모). 새 세션 탭이 어느 클로드를
/// 띄웠는지 가를 때만 쓴다 — 몇 ms 걸리므로 늘 부르지 않는다.
pub(crate) fn process_parents() -> HashMap<u32, u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let mut map = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return map;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            map.insert(e.th32ProcessID, e.th32ParentProcessID);
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    map
}

/// pid가 root 아래(자식·손자…)에서 나왔는가. 번호가 재사용돼 고리가 생겨도 멈추게
/// 몇 단계만 올라간다(탭 → cmd.exe → claude 는 한두 단계다).
pub(crate) fn descends_from(pid: u32, root: u32, parents: &HashMap<u32, u32>) -> bool {
    if pid == root {
        return true;
    }
    let mut cur = pid;
    for _ in 0..8 {
        let Some(&p) = parents.get(&cur) else { return false };
        if p == root {
            return true;
        }
        if p == 0 || p == cur {
            return false;
        }
        cur = p;
    }
    false
}

/// 세션을 가져간 프로세스를 끝낸다. 우리가 띄우지 않은 프로세스라, 사용자가 띠에서
/// 직접 누를 때만 부른다. 번호 하나로 아무 프로세스나 끄게 두지 않는다 — 클로드가 쓴
/// 상태 파일이 "그 프로세스가 이 탭의 세션을 쥐고 있다"고 말하고, 그 파일이 적어 둔
/// 시작 시각이 지금 그 번호를 쓰는 프로세스와 같을 때만 끈다. 비정상 종료로 남은
/// 파일이 있을 수 있어 파일만으로는 부족하다(번호는 재사용된다).
#[tauri::command(async)]
pub(crate) fn kill_session_owner(id: String, pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    let key = ACTIVITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(|a| a.session_id.clone())
        .unwrap_or(id);
    let home = dirs::home_dir().ok_or("홈 폴더를 찾을 수 없습니다")?;
    let path = home.join(".claude").join("sessions").join(format!("{pid}.json"));
    let text = fs::read_to_string(&path).map_err(|_| "그 프로세스는 이미 없습니다".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("상태 파일을 읽지 못했습니다: {e}"))?;
    if v["sessionId"].as_str() != Some(key.as_str()) {
        return Err("그 프로세스는 이 세션을 쥐고 있지 않습니다".into());
    }
    let want: Option<u64> = v["procStart"].as_str().and_then(|s| s.parse().ok());

    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return Err("그 프로세스를 열 수 없습니다 (이미 끝났을 수 있습니다)".into());
        }
        let started = process_start(h);
        // 시작 시각을 확인할 수 없으면 끄지 않는다. 엉뚱한 프로세스를 끄는 것보다
        // 못 끄는 쪽이 낫다 — 사용자는 탭을 닫고 다시 여는 길이 남아 있다.
        if want.is_none() || started.is_none() || want != started {
            CloseHandle(h);
            return Err("그 번호는 다른 프로세스가 쓰고 있습니다".into());
        }
        let ok = TerminateProcess(h, 1);
        CloseHandle(h);
        if ok == 0 {
            return Err("종료 요청이 거부됐습니다".into());
        }
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn spawn_pty(
    app: AppHandle,
    state: State<PtyState>,
    id: String,
    cwd: String,
    command: String,
    // 세션 파일 경로 — 턴 종료 판정용. 새 세션은 아직 파일이 없어 None.
    file: Option<String>,
    // 알림에 쓸 탭 제목
    title: Option<String>,
    // 새 세션 탭이 나중에 알게 된 진짜 세션 id. 재시작·다시 열기 때 다시 넘겨야 상태
    // 파일·세션 인수인계 감지가 이어진다(탭 id는 "new-…"로 그대로다).
    session_id: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<u64, String> {
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get(&id) {
        return Ok(p.generation); // 이미 실행 중
    }
    // 없는 폴더를 홈으로 바꿔 띄우면 안 된다. 사용자는 C:\workspace\game에서 시작한 줄
    // 아는데 에이전트는 홈에서 돌고, 세션 기록도 홈 프로젝트로 쌓여 탭과 짝이 안 맞는다.
    // cwd를 모르는 세션(빈 문자열)만 예전처럼 홈에서 띄운다.
    let workdir = if cwd.trim().is_empty() {
        dirs::home_dir().unwrap_or_default().to_string_lossy().to_string()
    } else if PathBuf::from(&cwd).is_dir() {
        cwd.clone()
    } else {
        return Err(format!("폴더가 없습니다: {cwd}"));
    };

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    // 최종 명령은 프런트에서 합성됨: [래퍼 접두사] + 에이전트 명령 + [--resume <세션ID>]
    let claude_cmd = if command.trim().is_empty() { "claude".to_string() } else { command };
    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.args(["/c", &claude_cmd]);
    cmd.cwd(&workdir);

    let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    let root_pid = child.process_id().unwrap_or(0);
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let mut writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let killer = child.clone_killer();
    // 쓰기 스레드 — 보내는 쪽(PtyInstance)이 사라지면 recv가 끝나 스스로 멈춘다
    let (input, input_rx) = std::sync::mpsc::channel::<PtyInput>();
    std::thread::spawn(move || {
        while let Ok(job) = input_rx.recv() {
            match job {
                PtyInput::Bytes(bytes) => {
                    if writer.write_all(&bytes).is_err() {
                        break; // 프로세스가 끝났다
                    }
                }
                PtyInput::Pause(d) => std::thread::sleep(d),
            }
        }
    });
    let generation = PTY_GENERATION.fetch_add(1, Ordering::Relaxed);

    let agent = trace_agent_label(&claude_cmd);
    trace(&id, &agent, "spawn", &redact_env(&claude_cmd));
    if PTY_TRACE.load(Ordering::Relaxed) {
        trace(&id, &agent, "conpty", &conpty_in_use());
    }

    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).insert(
        id.clone(),
        Activity {
            session_id: session_id.filter(|s| !s.trim().is_empty()),
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
            file_backed: false,
            waiting: false,
            owner_pid: None,
            moved: false,
            moved_seen: None,
            root_pid,
            pid_tries: 0,
            pid_found: 0,
            pid_session: None,
        },
    );

    // 한산할 때는 그대로 흘리고, 몰릴 때만 묶는다. 조각마다 IPC를 쏘면 3바이트짜리
    // 이벤트가 초당 수십 개 올라가고 그때마다 xterm이 다시 그린다 — 실측한 100ms대
    // longtask 358건이 전부 출력 직후에 있었다. 첫 조각은 즉시 보내 에코가 늦지 않게 하고,
    // 그 직후 몰려오는 조각만 FLUSH_MS 동안 모은다.
    let pipe = Arc::new(OutPipe::new());
    let flush_pipe = pipe.clone();
    let flush_app = app.clone();
    let flush_id = id.clone();
    let flushing = Arc::new(AtomicBool::new(true));
    let flush_flag = flushing.clone();
    std::thread::spawn(move || {
        let emit = |d: &[u8]| emit_output(&flush_app, &flush_id, generation, d);
        while flush_flag.load(Ordering::Relaxed) {
            // 모인 게 없으면 깨울 때까지 잔다 — 탭마다 초당 수십 번씩 헛깨는 타이머는 두지 않는다
            if !flush_pipe.wait_for_data(std::time::Duration::from_millis(500)) {
                continue;
            }
            // 잠금 밖에서 모을 시간을 준 뒤 한 번에 비운다
            std::thread::sleep(std::time::Duration::from_millis(FLUSH_MS));
            flush_pipe.flush(std::time::Instant::now(), &emit);
        }
    });

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
                    // 이 탭을 다시 열었으면 새 프로세스의 상태 항목에 잠깐 출력이 더해질 수 있다.
                    // 끈 프로세스는 PTY가 닫히면 곧 EOF라 창은 짧고, 결과도 잠깐의 "작업 중"
                    // 오판뿐이라 조각마다 세대를 확인하지 않는다. 화면에는 섞이지 않는다
                    // (출력은 실행 번호로 걸러진다).
                    note_output(&id2);
                    pipe.write(std::time::Instant::now(), &buf[..n], &|d: &[u8]| {
                        emit_output(&app2, &id2, generation, d)
                    });
                }
            }
        }
        // 남은 것을 마저 비우고 배출 스레드를 세운다
        pipe.drain(&|d: &[u8]| emit_output(&app2, &id2, generation, d));
        flushing.store(false, Ordering::Relaxed);
        pipe.wake();
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

    map.insert(id, PtyInstance { master: pair.master, input, killer, agent, generation });
    Ok(generation)
}

/// `sent`는 진단 기록이 켜져 있을 때 화면 쪽이 실어 보내는 벽시계 시각(ms) — 키가 이
/// 명령(메인 스레드)에 닿기까지 밀린 시간을 "in" 줄에 함께 남긴다.
#[tauri::command]
pub(crate) fn write_pty(state: State<PtyState>, id: String, data: String, sent: Option<f64>) -> Result<(), String> {
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get_mut(&id) {
        let value = match sent {
            Some(t) => format!("{},{:.1},{}", data.len(), wall_ms() - t, input_kind(data.as_bytes())),
            None => data.len().to_string(),
        };
        trace(&id, &p.agent, "in", &value);
        if let Some(a) = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
            if is_user_input(data.as_bytes()) {
                a.last_input = Some(std::time::Instant::now());
            }
            note_draft(a, data.as_bytes());
        }
        // 넘기기만 한다 — 쓰기가 막혀도 여기(메인 스레드, 전역 잠금)는 기다리지 않는다
        let _ = p.input.send(PtyInput::Bytes(data.into_bytes()));
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn resize_pty(state: State<PtyState>, id: String, cols: u16, rows: u16) -> Result<(), String> {
    let map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get(&id) {
        p.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn kill_pty(state: State<PtyState>, id: String) -> Result<(), String> {
    let removed = {
        let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
        let removed = map.remove(&id);
        ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        removed
    };
    // 끄고 치우는 건 잠금 밖에서 한다 (PTY를 닫는 동안 다른 탭이 기다리지 않게)
    if let Some(mut p) = removed {
        let _ = p.killer.kill();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// 탭(cmd.exe 10) 아래의 클로드(12)만 그 탭 것이다. 다른 탭(20)의 것, 고리가 생긴
    /// 번호(재사용), 모르는 번호는 아니다.
    #[test]
    fn descends_only_from_its_own_tab() {
        let parents: std::collections::HashMap<u32, u32> =
            [(10, 1), (11, 10), (12, 11), (20, 1), (21, 20), (30, 31), (31, 30)].into();
        assert!(super::descends_from(12, 10, &parents));
        assert!(super::descends_from(11, 10, &parents));
        assert!(!super::descends_from(21, 10, &parents));
        assert!(!super::descends_from(30, 10, &parents), "고리에서 멈춰야 한다");
        assert!(!super::descends_from(99, 10, &parents));
    }

    /// 지금 이 테스트 프로세스는 자기 부모 아래에 있다 — 실제 프로세스 목록으로 본다
    #[test]
    fn process_list_knows_this_process() {
        let parents = super::process_parents();
        let me = std::process::id();
        let parent = *parents.get(&me).expect("자기 자신이 목록에 없다");
        assert!(super::descends_from(me, parent, &parents));
    }

    /// 이미지 파일 경로를 괄호 붙여넣기로 보내면 클로드가 이미지로 첨부하는가
    /// (Ctrl+V로 캡처를 붙여넣을 때 앱이 하는 일). 제출하지 않으므로 요청은 나가지 않는다.
    /// `cargo test -- --ignored claude_attaches_pasted_image_path --nocapture`
    #[test]
    #[ignore]
    fn claude_attaches_pasted_image_path() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("deck-typeahead");
        std::fs::create_dir_all(&dir).unwrap();
        let img = std::env::temp_dir().join("cli-deck-paste-test.png");
        let px: Vec<u8> = (0..16 * 16).flat_map(|i| [(i % 255) as u8, 80, 160, 255]).collect();
        crate::paste::write_png_for_test(&img, &px, 16, 16);
        let pair = native_pty_system()
            .openpty(PtySize { rows: 30, cols: 120, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/c", "claude"]);
        cmd.cwd(&dir);
        for (k, _) in std::env::vars() {
            if k.starts_with("CLAUDE") || k == "AI_AGENT" {
                cmd.env_remove(k);
            }
        }
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() { break; }
            }
        });
        let t0 = std::time::Instant::now();
        let mut screen = Vec::new();
        let mut sent = false;
        while t0.elapsed() < std::time::Duration::from_secs(25) {
            if let Ok(c) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
                if String::from_utf8_lossy(&c).contains("[c") {
                    let _ = writer.write_all(b"\x1b[?1;2c");
                }
                screen.extend_from_slice(&c);
            }
            // 첫 화면이 뜬 뒤(6초) 경로를 괄호 붙여넣기로 보낸다 — xterm의 term.paste와 같다
            if !sent && t0.elapsed() > std::time::Duration::from_secs(6) {
                let paste = format!("\x1b[200~{}\x1b[201~", img.display());
                let _ = writer.write_all(paste.as_bytes());
                sent = true;
            }
            if sent && String::from_utf8_lossy(&screen).contains("[Image") {
                break;
            }
        }
        let _ = child.kill();
        let text = String::from_utf8_lossy(&screen);
        let attached = text.contains("[Image");
        let plain: String = text.chars().filter(|c| c.is_alphanumeric() || " #[]._-\\:".contains(*c)).collect();
        let tail: String = plain.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
        eprintln!("첨부됨: {attached}\n화면 끝: {tail}");
        let _ = std::fs::remove_file(&img);
        assert!(attached, "경로를 붙여넣었는데 이미지로 첨부되지 않았다");
    }

    /// 새 세션 탭의 클로드를 pid로 찾을 수 있는가: 상태 파일이 언제 생기고, 그 pid가
    /// 탭의 cmd.exe 아래에 있는가. 제출하지 않으므로 요청은 나가지 않는다.
    /// `cargo test -- --ignored new_tab_session_by_pid --nocapture`
    #[test]
    #[ignore]
    fn new_tab_session_by_pid() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("deck-typeahead");
        std::fs::create_dir_all(&dir).unwrap();
        let pair = native_pty_system()
            .openpty(PtySize { rows: 30, cols: 120, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/c", "claude"]);
        cmd.cwd(&dir);
        // 이 테스트를 클로드 안에서 돌리면 "자식 세션" 표시가 따라가 기록·상태 파일을
        // 안 쓴다. 앱이 띄울 때와 같게 걷어 낸다.
        for (k, _) in std::env::vars() {
            if k.starts_with("CLAUDE") || k == "AI_AGENT" {
                cmd.env_remove(k);
            }
        }
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        let root = child.process_id().unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() { break; }
            }
        });
        let t0 = std::time::Instant::now();
        let mut found = None;
        let mut screen: Vec<u8> = Vec::new();
        while t0.elapsed() < std::time::Duration::from_secs(45) && found.is_none() {
            if let Ok(c) = rx.recv_timeout(std::time::Duration::from_millis(250)) {
                screen.extend_from_slice(&c);
                if String::from_utf8_lossy(&c).contains("[c") {
                    let _ = writer.write_all(b"\x1b[?1;2c");
                }
            }
            let (_, owner) = crate::activity::scan_session_status();
            let parents = super::process_parents();
            let mine: Vec<_> = owner.iter().filter(|(_, &pid)| super::descends_from(pid, root, &parents)).collect();
            if !mine.is_empty() {
                eprintln!("{}ms 뒤 찾음: {:?} (전체 주인 {}개)", t0.elapsed().as_millis(), mine, owner.len());
                found = Some(mine.len());
            }
        }
        if found.is_none() {
            let t = String::from_utf8_lossy(&screen);
            let plain: String = t.chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect();
            let tail: String = plain.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
            eprintln!("화면 끝: {tail}");
            let parents = super::process_parents();
            let (_, owner) = crate::activity::scan_session_status();
            eprintln!("root {root} 의 자식들: {:?}", parents.iter().filter(|(_, &p)| p == root).collect::<Vec<_>>());
            for (sid, pid) in &owner {
                let mut chain = vec![*pid];
                while let Some(&p) = parents.get(chain.last().unwrap()) {
                    if chain.len() > 6 || p == 0 { break; }
                    chain.push(p);
                }
                eprintln!("{sid} {chain:?}");
            }
        }
        let _ = child.kill();
        assert_eq!(found, Some(1), "탭 아래의 클로드 세션을 못 찾았다");
    }

    /// 클로드가 뜨는 동안 친 글자를 버리는가. 빈 임시 폴더에서 새 세션을 띄우고, 첫 화면
    /// 전에 글자를 보낸 뒤, 화면이 뜨고 나서 입력창에 남아 있는지 본다. 제출하지 않으므로
    /// 요청은 나가지 않는다. `cargo test -- --ignored claude_typeahead --nocapture`
    #[test]
    #[ignore]
    fn claude_typeahead() {
        // 이미 믿는 폴더(이 저장소) 안이어야 "이 폴더를 믿겠냐" 창이 안 뜬다. target은 git이 무시한다.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("deck-typeahead");
        std::fs::create_dir_all(&dir).unwrap();
        let pair = native_pty_system()
            .openpty(PtySize { rows: 30, cols: 120, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/c", "claude"]);
        cmd.cwd(&dir);
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() { break; }
            }
        });
        let t0 = std::time::Instant::now();
        let mut screen = Vec::new();
        let mut sent = false;
        let mut first_big = None;
        while t0.elapsed() < std::time::Duration::from_secs(25) {
            if let Ok(c) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
                // 터미널 질의에 답한다 (xterm처럼) — 안 하면 뜨는 게 늦어진다
                if c.windows(3).any(|w| w == b"[c ") || String::from_utf8_lossy(&c).contains("[c") {
                    let _ = writer.write_all(b"[?1;2c");
                }
                if c.len() > 1500 && first_big.is_none() {
                    first_big = Some(t0.elapsed().as_millis());
                }
                screen.extend_from_slice(&c);
            }
            // 띄우고 0.3초 뒤, 첫 화면 전에 글자를 친다
            if !sent && t0.elapsed() > std::time::Duration::from_millis(300) {
                let _ = writer.write_all(b"typeahead");
                sent = true;
                eprintln!("글자 보냄 {}ms (첫 큰 화면 전: {})", t0.elapsed().as_millis(), first_big.is_none());
            }
            if let Some(fb) = first_big {
                if t0.elapsed().as_millis() > fb + 3000 { break; }
            }
        }
        let text = String::from_utf8_lossy(&screen);
        // 이스케이프를 걷어 사람이 보는 글자만 — typeahead 앞뒤로 무엇이 그려졌는지
        let mut plain = String::new();
        let mut esc = false;
        for ch in text.chars() {
            if esc { if ch.is_ascii_alphabetic() || ch == '~' { esc = false; } continue; }
            if ch == '\x1b' { esc = true; continue; }
            if ch == '\r' { continue; }
            plain.push(ch);
        }
        if let Some(i) = plain.find("typeahead") {
            let lo = plain[..i].char_indices().rev().nth(200).map(|x| x.0).unwrap_or(0);
            let hi = (i + 300).min(plain.len());
            let hi = (hi..=plain.len()).find(|&k| plain.is_char_boundary(k)).unwrap_or(plain.len());
            eprintln!("--- typeahead 주변 ---\n{}\n---", &plain[lo..hi]);
        }
        eprintln!("--- 마지막 화면 ---\n{}\n---", plain.chars().rev().take(600).collect::<String>().chars().rev().collect::<String>());
        eprintln!("첫 큰 화면 {:?}ms, 화면에 typeahead가 {}", first_big,
                  if text.contains("typeahead") { "보인다(버리지 않음)" } else { "없다(버림)" });
        let _ = child.kill();
        drop(pair.master);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    /// 풀스크린 클로드가 보내던 크기(약 10KB)의 전체 화면을 쉬지 않고 쏟아낼 때
    /// ConPTY가 얼마나 빨리 통과시키는가. 느리면 클로드의 출력 쓰기가 막히고, 그동안
    /// 키 입력도 처리하지 못한다(실제로 입력 뒤 5~9초 무응답이 기록됐다).
    /// `cargo test -- --ignored conpty_frame_throughput --nocapture`
    #[test]
    #[ignore]
    fn conpty_frame_throughput() {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 42, cols: 176, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        // 176x42 화면을 줄마다 위치를 찍고 색을 바꿔 가며 다시 그린다 — 클로드의 프레임과 비슷한 모양
        let ps = "$e=[char]27; $sw=[Diagnostics.Stopwatch]::StartNew(); \
                  for($k=0;$k -lt 200;$k++){ $f=$e+'[?2026h'; \
                  for($y=1;$y -le 42;$y++){ $f+=$e+'['+$y+';1H'+$e+'['+(31+($y+$k)%7)+'m'+('가나다abc'+$k+' ')*14 } ; \
                  $f+=$e+'[0m'+$e+'[?2026l'; [Console]::Out.Write($f) }; [Console]::Out.Flush(); \
                  $sw.Stop(); [Console]::Out.Write($e+'[0mDONE '+$sw.ElapsedMilliseconds+' '+$f.Length)";
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.args(["-NoProfile", "-Command", ps]);
        let t0 = std::time::Instant::now();
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() { break; }
            }
        });
        let mut total = 0usize;
        let mut first: Option<f64> = None;
        let mut tail = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(1500)) {
                Ok(c) => {
                    if first.is_none() { first = Some(t0.elapsed().as_secs_f64() * 1000.0); }
                    total += c.len();
                    tail.extend_from_slice(&c);
                    if tail.len() > 4096 { tail.drain(..tail.len() - 4096); }
                    if String::from_utf8_lossy(&tail).contains("DONE ") { break; }
                }
                Err(_) => break,
            }
        }
        let all = t0.elapsed().as_secs_f64() * 1000.0;
        let s = String::from_utf8_lossy(&tail).to_string();
        let done = s.rsplit("DONE ").next().unwrap_or("").split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        eprintln!("자식이 200장 쓰는 데 걸린 시간·한 장 크기: {done}  /  우리가 받은 양 {} KB, 첫 조각 {:.0}ms, 끝까지 {all:.0}ms",
                  total / 1024, first.unwrap_or(0.0));
        let _ = child.kill();
    }

    /// spawn_pty·resize_pty·kill_pty는 메인 스레드에서 돈다. 여기서 막히면 창 전체가
    /// 멈추는데(출력 이벤트도 메인 스레드를 거친다) 화면 쪽 기록(longtask)에는 안 잡힌다.
    /// 각각 얼마나 걸리는지, 자식이 출력을 쏟아내는 중에도 그런지 잰다.
    /// `cargo test -- --ignored main_thread_pty_calls --nocapture`
    #[test]
    #[ignore]
    fn main_thread_pty_calls() {
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1000.0;
        for round in 0..3 {
            let t = std::time::Instant::now();
            let pair = native_pty_system()
                .openpty(PtySize { rows: 42, cols: 176, pixel_width: 0, pixel_height: 0 })
                .unwrap();
            let open_ms = ms(t);
            let mut cmd = CommandBuilder::new("cmd.exe");
            // 클로드가 시작하며 화면을 쏟아내는 것처럼 출력을 계속 낸다
            cmd.args(["/c", "for /L %i in (1,1,4000) do @echo ################################################################ %i"]);
            let t = std::time::Instant::now();
            let mut child = pair.slave.spawn_command(cmd).unwrap();
            let spawn_ms = ms(t);
            drop(pair.slave);
            let mut reader = pair.master.try_clone_reader().unwrap();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 { break; }
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(150));
            let mut resize = Vec::new();
            for k in 0..10 {
                let t = std::time::Instant::now();
                let _ = pair.master.resize(PtySize {
                    rows: 42 + (k % 2), cols: 176, pixel_width: 0, pixel_height: 0,
                });
                resize.push(ms(t));
            }
            let t = std::time::Instant::now();
            let _ = child.clone_killer().kill();
            let kill_ms = ms(t);
            let t = std::time::Instant::now();
            drop(pair.master);
            let close_ms = ms(t);
            let _ = child.wait();
            resize.sort_by(|a, b| a.partial_cmp(b).unwrap());
            eprintln!(
                "{round}: openpty {open_ms:.1}ms  spawn {spawn_ms:.1}ms  resize 중앙 {:.1} 최대 {:.1}ms  kill {kill_ms:.1}ms  PTY 닫기 {close_ms:.1}ms",
                resize[5], resize[9]
            );
        }
    }

    /// 탭을 닫거나 다시 열 때 우리가 끄는 건 `cmd.exe`다. 에이전트는 그 아래 손자라
    /// TerminateProcess가 닿지 않는다. 그래도 죽는지 — 안 죽으면 세션 하나에 프로세스가
    /// 계속 쌓인다. `cargo test -- --ignored grandchild_dies --nocapture`
    #[test]
    #[ignore]
    fn grandchild_dies_when_the_pty_closes() {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("cmd.exe");
        // ping은 오래 살아 있고 부모가 없어도 계속 돈다 — 손자가 남는지 보기 좋다
        cmd.args(["/c", "ping", "-n", "60", "127.0.0.1"]);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let cmd_pid = child.process_id().unwrap();
        // 함께 배포하는 ConPTY는 터미널이 장치 속성 질의에 답할 때까지(여기선 아무도 안 답해
        // 3초) 자식을 세워 둔다 — 그보다 오래 기다린다
        std::thread::sleep(std::time::Duration::from_millis(4000));

        let alive = |name: &str| -> usize {
            let out = std::process::Command::new("tasklist").args(["/NH"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).to_lowercase().matches(name).count()
        };
        let before = alive("ping.exe");
        assert!(before > 0, "ping이 떠 있어야 시험이 성립한다");

        // 앱이 탭을 닫을 때 하는 것과 같다: 직계 자식을 끄고 PTY를 놓는다
        let _ = child.clone_killer().kill();
        drop(child);
        drop(pair.master);
        std::thread::sleep(std::time::Duration::from_millis(2000));

        let after = alive("ping.exe");
        eprintln!("cmd.exe pid {cmd_pid} / ping 프로세스 {before}개 → {after}개");
        assert!(after < before, "손자가 살아남았다 — 다시 열면 세션에 프로세스가 하나 더 붙는다");
    }

    /// 함께 배포하는 ConPTY는 탭마다 OpenConsole.exe를 띄운다. 탭을 닫으면(자식을 끄고 PTY를
    /// 놓으면) 그것도 끝나는가 — 안 끝나면 닫은 탭마다 프로세스가 하나씩 남는다.
    /// conpty.dll·OpenConsole.exe를 시험 실행 파일 옆에 두고 돌린다.
    /// DECK_LEAVE_PTY=1이면 PTY를 쥔 채 프로세스가 끝난다(업데이트로 앱이 꺼질 때) — 밖에서 센다.
    /// `cargo test -- --ignored openconsole_exits_with_its_pty --nocapture`
    #[test]
    #[ignore]
    fn openconsole_exits_with_its_pty() {
        let mine = || -> usize {
            let dir = std::env::current_exe().unwrap().parent().unwrap().join("OpenConsole.exe");
            let ps = format!(
                "@(Get-Process OpenConsole -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{}' }}).Count",
                dir.display()
            );
            let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &ps]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
        };
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/c", "ping", "-n", "60", "127.0.0.1"]);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let running = mine();
        if std::env::var("DECK_LEAVE_PTY").is_ok() {
            eprintln!("PTY를 쥔 채 끝낸다 (떠 있는 것 {running}개)");
            std::process::exit(0);
        }
        let _ = child.clone_killer().kill();
        drop(child);
        drop(pair.master);
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let after = mine();
        eprintln!("OpenConsole {running}개 → PTY를 놓고 1초 뒤 {after}개");
        assert!(running > 0, "OpenConsole이 안 떴다 — 시험 실행 파일 옆에 conpty.dll이 있는가");
        assert_eq!(after, 0, "PTY를 놓았는데 OpenConsole이 남았다");
    }

    /// 넓은 글자(한글·이모지)가 ConPTY를 지나며 바뀌는가. 내장 conhost는 화면을 자기 폭 표로 다시
    /// 그려 보내고, 함께 배포하는 ConPTY는 그대로 넘긴다 — 어느 쪽이든 글자와 그 뒤 위치가
    /// 살아 와야 한다. 한글을 입력으로 넣어 그대로 돌아오는지도 본다.
    /// `cargo test -- --ignored conpty_wide_text --nocapture`
    #[test]
    #[ignore]
    fn conpty_wide_text() {
        let line = "한글 입력 ⚠️ ✔️ 🥲 👍🏻 👨‍💻 🫠 끝";
        let js = r#"
const E = '\x1b';
process.stdin.setRawMode(true); process.stdin.resume();
process.stdout.write(E + '[2J' + E + '[5;1H' + process.env.DECK_LINE + E + '[5;70HEND' + E + '[7;1H');
process.stdin.on('data', (d) => { process.stdout.write('IN[' + d.toString('utf8') + ']'); });
setTimeout(() => process.exit(0), 8000);
"#;
        let sideloaded = std::env::current_exe().unwrap().parent().unwrap().join("conpty.dll").exists();
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("node");
        cmd.args(["-e", js]);
        cmd.env("DECK_LINE", line);
        let _child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let w2 = writer.clone();
        let got = Arc::new(Mutex::new(Vec::<u8>::new()));
        let g2 = got.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                if String::from_utf8_lossy(&buf[..n]).contains("\x1b[c") {
                    let _ = w2.lock().unwrap().write_all(b"\x1b[?1;2c");
                }
                g2.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(2500));
        writer.lock().unwrap().write_all("한글".as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let all = String::from_utf8_lossy(&got.lock().unwrap()).to_string();
        // 이스케이프를 걷어 보이는 글자만
        let mut plain = String::new();
        let mut esc = false;
        for ch in all.chars() {
            if esc { if ch.is_ascii_alphabetic() || ch == '~' { esc = false; } continue; }
            if ch == '\x1b' { esc = true; continue; }
            plain.push(ch);
        }
        let verbatim = all.contains(line);
        let words_ok = ["한글", "입력", "🥲", "👨\u{200d}💻", "🫠", "끝", "END"].iter().all(|w| plain.contains(w));
        let echo_ok = plain.contains("IN[한글]");
        eprintln!(
            "{}: 줄 그대로 {verbatim}, 글자 모두 {words_ok}, 한글 입력 그대로 {echo_ok}\n보인 것: {:?}",
            if sideloaded { "conpty.dll" } else { "내장 conhost" },
            plain.trim()
        );
        assert!(words_ok && echo_ok);
    }

    /// 글자가 아닌 입력(ESC·Ctrl+C·방향키·Shift+Tab·마우스 휠/움직임·포커스·괄호 붙여넣기)이
    /// ConPTY를 지나 앱에 그대로 닿는가. 받은 바이트를 JSON으로 찍게 해 내장과 함께 배포하는
    /// ConPTY를 비교한다. `cargo test -- --ignored conpty_control_input --nocapture`
    #[test]
    #[ignore]
    fn conpty_control_input() {
        let js = r#"
process.stdin.setRawMode(true); process.stdin.resume();
process.stdout.write('\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004hREADY\r\n');
process.stdin.on('data', (d) => { process.stdout.write('GOT' + JSON.stringify(d.toString('latin1')) + '\r\n'); });
setTimeout(() => process.exit(0), 12000);
"#;
        let sends: [(&str, &[u8]); 9] = [
            ("ESC", b"\x1b"),
            ("Ctrl+C", b"\x03"),
            ("Up", b"\x1b[A"),
            ("Shift+Tab", b"\x1b[Z"),
            ("wheel", b"\x1b[<64;10;5M"),
            ("motion", b"\x1b[<35;12;6M"),
            ("focus-in", b"\x1b[I"),
            ("paste", b"\x1b[200~hi\x1b[201~"),
            ("Enter", b"\r"),
        ];
        let sideloaded = std::env::current_exe().unwrap().parent().unwrap().join("conpty.dll").exists();
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("node");
        cmd.args(["-e", js]);
        let _child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let w2 = writer.clone();
        let got = Arc::new(Mutex::new(String::new()));
        let g2 = got.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                let s = String::from_utf8_lossy(&buf[..n]).to_string();
                if s.contains("\x1b[c") {
                    let _ = w2.lock().unwrap().write_all(b"\x1b[?1;2c");
                }
                g2.lock().unwrap().push_str(&s);
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(2000));
        eprintln!("{}", if sideloaded { "conpty.dll" } else { "내장 conhost" });
        for (name, bytes) in sends {
            got.lock().unwrap().clear();
            writer.lock().unwrap().write_all(bytes).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
            let s = got.lock().unwrap().clone();
            let recv: Vec<&str> = s.split("GOT").skip(1).map(|x| x.split('\r').next().unwrap_or("")).collect();
            eprintln!("  {name:<10} 보냄 {:<24} 받음 {}", format!("{:?}", String::from_utf8_lossy(bytes)), recv.join(" "));
        }
    }

    #[test]
    fn input_kind_tells_wheel_motion_and_clicks_apart() {
        assert_eq!(input_kind(b"\x1b[<64;10;5M"), 'w'); // 휠 위
        assert_eq!(input_kind(b"\x1b[<65;10;5M"), 'w'); // 휠 아래
        assert_eq!(input_kind(b"\x1b[<35;10;5M"), 'm'); // 버튼 없이 움직임
        assert_eq!(input_kind(b"\x1b[<0;10;5M"), 'c');
        assert_eq!(input_kind(b"\x1b[<0;10;5m"), 'c');
        assert_eq!(input_kind(b"a"), 'k');
        assert_eq!(input_kind(b"\x1b[A"), 'k');
    }

    /// 한산할 때의 첫 조각은 그대로 나가고(에코가 늦으면 타자가 밀린다), 그 직후
    /// 몰려오는 조각은 타이머가 한 번에 비운다.
    #[test]
    fn bursts_are_merged_but_the_first_chunk_is_not_delayed() {
        let t0 = std::time::Instant::now();
        let mut b = OutBuf::new();
        assert_eq!(b.push(t0, b"a").as_deref(), Some(&b"a"[..]), "첫 조각은 즉시");

        let t1 = t0 + std::time::Duration::from_millis(1);
        assert!(b.push(t1, b"b").is_none(), "직후 조각은 모은다");
        assert!(b.push(t1, b"c").is_none());
        assert_eq!(b.take(t1).as_deref(), Some(&b"bc"[..]), "타이머가 한 번에 비운다");
        assert!(b.take(t1).is_none(), "비울 게 없으면 이벤트도 없다");

        // 조용해진 뒤 다시 온 조각은 또 즉시 나간다
        let t2 = t1 + std::time::Duration::from_millis(FLUSH_MS);
        assert_eq!(b.push(t2, b"d").as_deref(), Some(&b"d"[..]));
    }

    /// 순서가 뒤집히던 원인은 "잠금을 놓고 보낸다"였다. 배출이 그 사이에 밀리면
    /// 읽기 쪽이 조용하다고 보고 다음 조각을 먼저 내보낸다. 스케줄링에 기대지 않고,
    /// 보내는 동안 잠금을 쥐고 있는지를 직접 확인한다.
    #[test]
    fn output_is_emitted_while_the_lock_is_held() {
        let pipe = Arc::new(OutPipe::new());
        let seen = Arc::new(AtomicBool::new(false));

        let p1 = pipe.clone();
        let s1 = seen.clone();
        let check = move |_: &[u8]| {
            assert!(p1.locked_now(), "보내는 동안 잠금이 풀려 있으면 순서가 뒤집힌다");
            s1.store(true, Ordering::Relaxed);
        };

        let t = std::time::Instant::now();
        pipe.write(t, b"first", &check);                 // 한산할 때의 즉시 전송 경로
        assert!(seen.swap(false, Ordering::Relaxed));

        pipe.write(t, b"more", &|_: &[u8]| unreachable!("직후 조각은 모아 둔다"));
        pipe.flush(t, &check);                            // 배출 경로
        assert!(seen.swap(false, Ordering::Relaxed));

        pipe.write(t, b"tail", &|_: &[u8]| {});
        pipe.drain(&check);                               // 종료 시 배출 경로
        assert!(seen.load(Ordering::Relaxed));
    }

    /// 출력이 쏟아지면 타이머를 기다리지 않고 끊어 보낸다 — 메모리도, 한 번에 그리는
    /// 양도 무한정 불어나면 안 된다.
    #[test]
    fn a_flood_is_cut_at_the_cap() {
        let t = std::time::Instant::now();
        let mut b = OutBuf::new();
        b.push(t, b"x");                       // 첫 조각으로 last_emit이 잡힌다
        let big = vec![b'y'; FLUSH_MAX];
        let out = b.push(t, &big).expect("상한을 넘으면 바로 나간다");
        assert_eq!(out.len(), FLUSH_MAX);
        assert!(b.drain().is_none());
    }

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

    /// 실측용: 실행 직후 첫 출력까지 걸리는 시간. 사이드로드한 conpty.dll이 탭 여는
    /// 속도를 늦추는지 본다. `cargo test -- --ignored conpty_first_output --nocapture`
    #[test]
    #[ignore]
    fn conpty_first_output() {
        for round in 0..3 {
            let t0 = std::time::Instant::now();
            let pair = native_pty_system()
                .openpty(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
                .unwrap();
            let opened = t0.elapsed().as_millis();
            let mut cmd = CommandBuilder::new("cmd.exe");
            cmd.args(["/c", "echo READY&& ping -n 2 127.0.0.1 >nul"]);
            let mut child = pair.slave.spawn_command(cmd).unwrap();
            drop(pair.slave);
            let spawned = t0.elapsed().as_millis();
            let mut reader = pair.master.try_clone_reader().unwrap();
            let mut writer = pair.master.take_writer().unwrap();
            // xterm.js처럼 장치 속성 질의(DA1, ESC [ c)에 답한다. 답하지 않으면 새 ConPTY는
            // 시작을 몇 초 미룬다 — 실제 앱에서는 xterm이 답하므로 그 조건을 흉내 낸다.
            let answer = std::env::var("DECK_ANSWER_DA1").is_ok();
            let (tx, rx) = std::sync::mpsc::channel::<u128>();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut seen = Vec::new();
                let mut answered = false;
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 { break; }
                    if seen.is_empty() {
                        eprintln!("   첫 조각 {:?}", String::from_utf8_lossy(&buf[..n]));
                    }
                    seen.extend_from_slice(&buf[..n]);
                    if answer && !answered && String::from_utf8_lossy(&seen).contains("\x1b[c") {
                        let _ = writer.write_all(b"\x1b[?1;2c");
                        let _ = writer.flush();
                        answered = true;
                    }
                    if String::from_utf8_lossy(&seen).contains("READY") {
                        let _ = tx.send(t0.elapsed().as_millis());
                        break;
                    }
                }
            });
            let ready = rx.recv_timeout(std::time::Duration::from_secs(15)).ok();
            let _ = child.kill();
            eprintln!("#{round} openpty {opened}ms, spawn {spawned}ms, 첫 출력 READY {:?}ms", ready);
        }
    }

    /// 실측용: 풀스크린 TUI처럼 동기화 출력(?2026h ... ?2026l)으로 감싼 큰 프레임을
    /// ConPTY에 흘려 보내고, 마스터 쪽에서 괄호가 살아 나오는지와 몇 조각으로 쪼개져
    /// 오는지 본다. `cargo test -- --ignored conpty_sync_output --nocapture`
    #[test]
    #[ignore]
    fn conpty_sync_output() {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let ps = "$e=[char]27; $l=('#'*110); \
                  for($k=0;$k -lt 5;$k++){ $f=$e+'[?2026h'+$e+'[H'; \
                  for($i=0;$i -lt 38;$i++){ $f+=$e+'['+($i+1)+';1H'+$k+$l } ; \
                  $f+=$e+'[?2026l'; [Console]::Out.Write($f); [Console]::Out.Flush(); Start-Sleep -Milliseconds 300 }";
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.args(["-NoProfile", "-Command", ps]);
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<(u128, Vec<u8>)>();
        let t0 = std::time::Instant::now();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send((t0.elapsed().as_millis(), buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        });
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut all = Vec::new();
        while let Ok((ms, chunk)) = rx.try_recv() {
            let s = String::from_utf8_lossy(&chunk);
            let head: String = s
                .chars()
                .take(160)
                .map(|c| if c == '\x1b' { "^".to_string() } else { c.to_string() })
                .collect();
            eprintln!(
                "{ms:>5}ms {:>6} bytes  2026h:{} 2026l:{}  {head}",
                chunk.len(),
                s.matches("?2026h").count(),
                s.matches("?2026l").count()
            );
            all.extend_from_slice(&chunk);
        }
        let s = String::from_utf8_lossy(&all);
        eprintln!(
            "합계 {} bytes, ?2026h {}개, ?2026l {}개 (보낸 것: 각 5개)",
            all.len(),
            s.matches("?2026h").count(),
            s.matches("?2026l").count()
        );
    }

    /// 실측용: 화면을 계속 다시 그리는 TUI(클로드처럼 마우스 모드·동기화 출력)에 키를 보내고,
    /// 그 앱이 받자마자 바꾼 창 제목(OSC 0)이 마스터 쪽에 나올 때까지의 시간을 잰다.
    /// 앱은 즉시 답하므로 이 시간은 전부 ConPTY 몫이다. 제목을 쓰는 건 ConPTY가 화면은
    /// 바뀐 칸만 다시 그려 보내지만 제목은 통째로 넘겨서다.
    /// DECK_FRAME_MS(다시 그리는 간격, 0이면 안 그림)·DECK_FRAME_ROWS(한 번에 그리는 줄 수).
    /// exe 옆에 conpty.dll·OpenConsole.exe가 있으면 portable-pty가 그쪽을 쓴다 — 둘을 비교한다.
    /// `cargo test -- --ignored conpty_input_latency --nocapture`
    #[test]
    #[ignore]
    fn conpty_input_latency() {
        let frame_ms: u64 = std::env::var("DECK_FRAME_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(33);
        let frame_rows: u32 = std::env::var("DECK_FRAME_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(42);
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let sideloaded = exe_dir.join("conpty.dll").exists();
        let js = r#"
const E = '\x1b', cols = 176, rows = +process.env.DECK_FRAME_ROWS, fms = +process.env.DECK_FRAME_MS;
process.stdin.setRawMode(true); process.stdin.resume();
process.stdout.write(E + '[?1049h' + E + '[?1000h' + E + '[?1006h');
let n = 0;
process.stdin.on('data', () => { n++; process.stdout.write(E + ']0;K' + n + '\x07'); });
let k = 0;
function frame() {
  let f = E + '[?2026h';
  for (let y = 0; y < rows; y++) {
    f += E + '[' + (1 + (y + k) % 42) + ';1H' + E + '[' + (31 + (y + k) % 7) + 'm' + ('abcdefgh' + k + ' ').repeat(16).slice(0, cols - 1);
  }
  f += E + '[0m' + E + '[?2026l'; k++;
  process.stdout.write(f);
}
if (fms > 0) setInterval(frame, fms);
setTimeout(() => process.exit(0), 60000);
"#;
        let pair = native_pty_system()
            .openpty(PtySize { rows: 42, cols: 176, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("node");
        cmd.args(["-e", js]);
        cmd.env("DECK_FRAME_MS", frame_ms.to_string());
        cmd.env("DECK_FRAME_ROWS", frame_rows.to_string());
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        // 제목 번호가 보일 때마다 그 시각을 남긴다
        let seen: Arc<Mutex<HashMap<u32, std::time::Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (seen2, bytes2) = (seen.clone(), bytes.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            let mut tail: Vec<u8> = Vec::new();
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                let now = std::time::Instant::now();
                bytes2.fetch_add(n, Ordering::Relaxed);
                tail.extend_from_slice(&buf[..n]);
                let s = String::from_utf8_lossy(&tail).to_string();
                let mut rest = s.as_str();
                while let Some(i) = rest.find("]0;K") {
                    let after = &rest[i + 4..];
                    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if digits.is_empty() || after.len() == digits.len() { break; }
                    if let Ok(k) = digits.parse::<u32>() {
                        seen2.lock().unwrap().entry(k).or_insert(now);
                    }
                    rest = &after[digits.len()..];
                }
                let keep = tail.len().saturating_sub(64);
                tail.drain(..keep);
            }
        });
        // 첫 화면이 자리 잡을 때까지 기다린 뒤 100ms마다 한 글자씩
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let b0 = bytes.load(Ordering::Relaxed);
        let t_start = std::time::Instant::now();
        let mut lat = Vec::new();
        let mut lost = 0;
        for i in 1..=60u32 {
            let sent = std::time::Instant::now();
            writer.write_all(b"a").unwrap();
            let _ = writer.flush();
            let deadline = sent + std::time::Duration::from_secs(5);
            loop {
                if let Some(t) = seen.lock().unwrap().get(&i) {
                    lat.push(t.duration_since(sent).as_secs_f64() * 1000.0);
                    break;
                }
                if std::time::Instant::now() > deadline { lost += 1; break; }
                std::thread::sleep(std::time::Duration::from_micros(300));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let kbps = (bytes.load(Ordering::Relaxed) - b0) as f64 / 1024.0 / t_start.elapsed().as_secs_f64();
        // 가장 늦은 것이 몇 번째였나 — 첫 입력만 늦으면 ConPTY가 시작할 때 터미널 답을 기다린 것
        let slow: Vec<usize> = lat.iter().enumerate().filter(|(_, &l)| l > 100.0).map(|(i, _)| i + 1).collect();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| lat.get(((lat.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(f64::NAN);
        eprintln!(
            "{} frame {}ms x{}줄  받은 출력 {:.0}KB/s  입력→반응 p50 {:.1} p90 {:.1} max {:.1} ms  (놓침 {lost}, 100ms 넘은 차례 {slow:?})",
            if sideloaded { "conpty.dll" } else { "내장 conhost" },
            frame_ms, frame_rows, kbps, q(0.5), q(0.9), q(1.0)
        );
        let _ = child.kill();
    }

    /// 실측용: 클로드가 답을 쓰는 동안 입력창에 친 글자가 화면에 돌아오기까지. 앱 탭과 같게
    /// cmd.exe /c로 띄우고 haiku에게 숫자를 길게 세게 한다(요청이 하나 나간다). 표시 글자 Ж는
    /// 답에 안 나오므로 그 글자 수가 늘면 에코가 온 것이다. DECK_XTVERSION=1이면 XTVERSION
    /// 질의에 xterm.js라고 답한다 — 클로드가 xterm.js용 처리를 켜는지 비교한다.
    /// `cargo test -- --ignored claude_echo_while_working --nocapture`
    #[test]
    #[ignore]
    fn claude_echo_while_working() {
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        let sideloaded = exe_dir.join("conpty.dll").exists();
        let xtversion = std::env::var("DECK_XTVERSION").map(|v| v == "1").unwrap_or(false);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("deck-typeahead");
        std::fs::create_dir_all(&dir).unwrap();
        let pair = native_pty_system()
            .openpty(PtySize { rows: 42, cols: 160, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/c", "claude", "--model", "haiku",
                  "Write a 2000-word short story about a lighthouse keeper. Plain text, no tools."]);
        cmd.cwd(&dir);
        for (k, _) in std::env::vars() {
            if k.starts_with("CLAUDE") || k == "AI_AGENT" {
                cmd.env_remove(k);
            }
        }
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let marks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (marks2, bytes2, writer2) = (marks.clone(), bytes.clone(), writer.clone());
        let queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let queries2 = queries.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 { break; }
                bytes2.fetch_add(n, Ordering::Relaxed);
                if let Ok(p) = std::env::var("DECK_DUMP") {
                    use std::io::Write as _;
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                        let _ = f.write_all(&buf[..n]);
                    }
                }
                let s = String::from_utf8_lossy(&buf[..n]).to_string();
                marks2.fetch_add(s.matches('Ж').count(), Ordering::Relaxed);
                // 터미널 질의에 xterm처럼 답한다
                let mut w = writer2.lock().unwrap();
                if s.contains("\x1b[c") || s.contains("\x1b[0c") {
                    let _ = w.write_all(b"\x1b[?1;2c");
                    queries2.lock().unwrap().push("DA1".into());
                }
                if s.contains("\x1b[>q") || s.contains("\x1b[>0q") {
                    queries2.lock().unwrap().push("XTVERSION".into());
                    if xtversion {
                        let _ = w.write_all(b"\x1bP>|xterm.js(6.0.0)\x1b\\");
                    }
                }
            }
        });
        // 답을 쓰기 시작할 때까지 기다린다
        std::thread::sleep(std::time::Duration::from_secs(6));
        let b0 = bytes.load(Ordering::Relaxed);
        let t_start = std::time::Instant::now();
        let mut lat = Vec::new();
        let mut lost = 0;
        for _ in 0..80 {
            let before = marks.load(Ordering::Relaxed);
            let sent = std::time::Instant::now();
            {
                let mut w = writer.lock().unwrap();
                w.write_all("Ж".as_bytes()).unwrap();
                let _ = w.flush();
            }
            let deadline = sent + std::time::Duration::from_secs(5);
            loop {
                if marks.load(Ordering::Relaxed) > before {
                    lat.push(sent.elapsed().as_secs_f64() * 1000.0);
                    break;
                }
                if std::time::Instant::now() > deadline { lost += 1; break; }
                std::thread::sleep(std::time::Duration::from_micros(300));
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
        let kbps = (bytes.load(Ordering::Relaxed) - b0) as f64 / 1024.0 / t_start.elapsed().as_secs_f64();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| lat.get(((lat.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(f64::NAN);
        let mut qs = queries.lock().unwrap().clone();
        qs.dedup();
        eprintln!(
            "{}{} 출력 {:.0}KB/s  에코 p50 {:.1} p75 {:.1} p90 {:.1} max {:.1} ms (놓침 {lost}, 질의 {:?})",
            if sideloaded { "conpty.dll" } else { "내장 conhost" },
            if xtversion { " +XTVERSION" } else { "" },
            kbps, q(0.5), q(0.75), q(0.9), q(1.0), qs
        );
        let _ = child.kill();
    }
}
