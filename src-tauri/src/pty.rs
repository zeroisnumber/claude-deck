// PTY 생성·입출력·종료

use super::*;
use std::sync::{Arc, Condvar};

// ---------- PTY 관리 ----------

pub(crate) struct PtyInstance {
    pub(crate) master: Box<dyn MasterPty + Send>,
    pub(crate) writer: Box<dyn Write + Send>,
    pub(crate) killer: Box<dyn ChildKiller + Send + Sync>,
    /// 트레이스 라벨 (claude/codex/gemini) — write_pty에서 입력 이벤트를 찍을 때 씀
    pub(crate) agent: String,
    /// 같은 id로 재시작(kill 직후 spawn)했을 때, 죽은 이전 프로세스의 대기 스레드가
    /// 새 인스턴스를 지워버리지 않도록 구분하는 세대 번호
    pub(crate) generation: u64,
}

pub(crate) static PTY_GENERATION: AtomicU64 = AtomicU64::new(0);

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
    drop(map);
    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    let _ = app.emit("pty-exit", PtyExit { id: id.to_string() });
}

#[derive(Default)]
pub(crate) struct PtyState(pub(crate) Mutex<HashMap<String, PtyInstance>>);

#[derive(Clone, Serialize)]
pub(crate) struct PtyOutput {
    pub(crate) id: String,
    pub(crate) data: String, // base64
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

fn emit_output(app: &AppHandle, id: &str, data: &[u8]) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let _ = app.emit("pty-output", PtyOutput { id: id.to_string(), data: encoded });
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
            session_id: None,
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
        let emit = |d: &[u8]| emit_output(&flush_app, &flush_id, d);
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
                    note_output(&id2);
                    pipe.write(std::time::Instant::now(), &buf[..n], &|d: &[u8]| {
                        emit_output(&app2, &id2, d)
                    });
                }
            }
        }
        // 남은 것을 마저 비우고 배출 스레드를 세운다
        pipe.drain(&|d: &[u8]| emit_output(&app2, &id2, d));
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

    map.insert(id, PtyInstance { master: pair.master, writer, killer, agent, generation });
    Ok(())
}

#[tauri::command]
pub(crate) fn write_pty(state: State<PtyState>, id: String, data: String) -> Result<(), String> {
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
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut p) = map.remove(&id) {
        let _ = p.killer.kill();
    }
    ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
