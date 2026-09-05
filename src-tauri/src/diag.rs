// 진단 — 크래시 로그와 PTY 트레이스

use super::*;

// ---------- 크래시 진단 ----------
// windows_subsystem="windows"(릴리스 빌드)는 콘솔이 없어 패닉 메시지(stderr)가
// 그냥 사라진다 — "가끔 팅긴다"는 게 이거였을 가능성이 높음. 패닉 시 로그 파일에
// 기록하고 네이티브 팝업을 띄워 최소한 원인을 알 수 있게 한다.
pub(crate) fn install_panic_hook() {
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
pub(crate) static PTY_TRACE: AtomicBool = AtomicBool::new(false);

/// 마커 파일이 있으면 재시작 후에도 켜진 상태가 유지된다 (환경변수는 그걸 설정한
/// 셸에서 띄웠을 때만 붙어서 놓치기 쉬움)
pub(crate) fn trace_marker_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("com.user.cli-deck").join("TRACE"))
}

pub(crate) fn init_trace() {
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
pub(crate) fn clear_diagnostics() -> Result<String, String> {
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
pub(crate) fn trace_ui(kind: String, value: String) {
    if kind.ends_with("-failed") {
        trace_always("ui", "webview", &kind, &value);
    } else {
        trace("ui", "webview", &kind, &value);
    }
}

#[tauri::command]
pub(crate) fn trace_enabled() -> bool {
    PTY_TRACE.load(Ordering::Relaxed)
}

/// 설정에서 켜고 끄기. 마커 파일로 상태를 남겨 재시작 후에도 유지된다.
/// 반환값은 로그 파일 경로 (설정 창에 표시).
#[tauri::command]
pub(crate) fn set_trace(enabled: bool) -> Result<String, String> {
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

pub(crate) static TRACE_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// 며칠 켜둬도 디스크를 잡아먹지 않도록. 샘플링 대신 상한으로 끊는다 —
/// 청크 간격 분포가 신호 자체라 솎아내면 데이터가 망가진다.
pub(crate) const TRACE_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// ccmanager가 Claude 감지에 쓰는 스피너 문자 집합
pub(crate) const SPINNER_GLYPHS: &str = "✱✲✳✴✵✶✷✸✹✺✻✼✽✾✿❀❁❂❃❇❈❉❊❋✢✣✤✥✦✧✨⊛⊕⊙◉◎◍⁂⁕※⍟☼★☆·•⏺▸▹∙⋅○●";

// 기록은 별도 스레드가 맡는다. 예전엔 청크마다 리더 스레드에서 바로 파일에 썼는데,
// 실측 결과 초당 137번의 쓰기 시스템 콜이 났다 — 그 스레드는 터미널 출력을 배달하는
// 스레드라, 파일 시스템이나 백신이 한 번 붙들면 그동안 화면이 멈춘다.
// 이제 핫패스는 채널에 문자열 하나 보내고 끝이고, 실제 쓰기는 모아서 처리한다.
pub(crate) static TRACE_TX: LazyLock<std::sync::mpsc::Sender<String>> = LazyLock::new(|| {
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

pub(crate) fn trace_log_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("com.user.cli-deck").join("pty-trace.log"))
}

pub(crate) fn trace(id: &str, agent: &str, kind: &str, value: &str) {
    trace_inner(id, agent, kind, value, false);
}

/// 창을 못 띄운 것처럼 앱을 못 쓰게 만드는 실패는 진단 기록이 꺼져 있어도 남긴다.
/// 원인을 알려면 로그가 필요한데, 로그를 켜려면 창이 열려야 하는 순환에 빠진다.
pub(crate) fn trace_always(id: &str, agent: &str, kind: &str, value: &str) {
    trace_inner(id, agent, kind, value, true);
}

pub(crate) fn trace_inner(id: &str, agent: &str, kind: &str, value: &str, force: bool) {
    if !force && !PTY_TRACE.load(Ordering::Relaxed) {
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
pub(crate) fn redact_env(cmd: &str) -> String {
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
pub(crate) fn trace_agent_label(command: &str) -> String {
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
pub(crate) fn has_spinner_glyph(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).chars().any(|c| SPINNER_GLYPHS.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
