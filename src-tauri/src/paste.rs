//! 클립보드에 글자 없이 이미지만 있을 때(Win+Shift+S 캡처 등) Ctrl+V로 붙여넣는다.
//! 터미널로는 이미지를 보낼 수 없으므로 임시 폴더에 PNG로 저장하고 그 경로를 돌려준다 —
//! 프런트가 경로를 붙여넣으면 클로드는 이미지로 첨부하고, 다른 에이전트도 파일로 읽는다.
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

/// 붙여넣은 이미지를 두는 곳. 하루 지난 것은 저장할 때마다 치운다.
fn paste_dir() -> PathBuf {
    std::env::temp_dir().join("cli-deck-paste")
}

const KEEP_SECS: u64 = 24 * 3600;

/// 클립보드 이미지를 PNG로 저장하고 경로를 돌려준다. 이미지가 없으면 None.
/// 메인 스레드에서 읽으면 안 된다(플러그인 경고) — async 명령이라 작업 스레드에서 돈다.
#[tauri::command(async)]
pub(crate) fn save_clipboard_image(app: AppHandle) -> Result<Option<String>, String> {
    let Ok(img) = app.clipboard().read_image() else { return Ok(None) };
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Ok(None);
    }
    let dir = paste_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    cleanup(&dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("paste-{stamp}.png"));
    write_png(&path, &opaque_if_blank(img.rgba()), w, h)?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// 32비트 비트맵을 내놓는 프로그램 중에는 알파 칸을 안 쓰고 0으로 두는 것이 있다
/// (문서상 BI_RGB면 그 바이트는 "쓰지 않음"이다). arboard는 그 알파를 믿으므로 그대로
/// 저장하면 완전히 투명한 그림이 되어 에이전트가 빈 이미지를 받는다. 실제 캡처가 모든
/// 픽셀이 투명할 리는 없으니, 그럴 때만 불투명으로 바꾼다.
fn opaque_if_blank(rgba: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if rgba.chunks_exact(4).all(|p| p[3] == 0) {
        let mut v = rgba.to_vec();
        v.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        std::borrow::Cow::Owned(v)
    } else {
        std::borrow::Cow::Borrowed(rgba)
    }
}

fn write_png(path: &Path, rgba: &[u8], w: u32, h: u32) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    // 큰 캡처(4K)도 금방 끝나게 압축은 빠른 쪽으로
    enc.set_compression(png::Compression::Fast);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(rgba).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_png_for_test(path: &Path, rgba: &[u8], w: u32, h: u32) {
    write_png(path, rgba, w, h).unwrap();
}

fn cleanup(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for e in entries.flatten() {
        let p = e.path();
        let is_ours = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("paste-") && n.ends_with(".png"));
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() > KEEP_SECS);
        if is_ours && old {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_a_readable_png() {
        let dir = std::env::temp_dir().join(format!("deck-paste-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paste-1.png");
        // 2×1: 빨강, 반투명 파랑
        super::write_png(&path, &[255, 0, 0, 255, 0, 0, 255, 128], 2, 1).unwrap();
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..8], &[255, 0, 0, 255, 0, 0, 255, 128]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blank_alpha_becomes_opaque() {
        // 알파를 0으로 둔 비트맵 → 불투명으로
        assert_eq!(&*super::opaque_if_blank(&[10, 20, 30, 0, 40, 50, 60, 0]), &[10, 20, 30, 255, 40, 50, 60, 255]);
        // 일부라도 알파가 있으면 진짜 투명도다 — 건드리지 않는다
        assert_eq!(&*super::opaque_if_blank(&[10, 20, 30, 0, 40, 50, 60, 128]), &[10, 20, 30, 0, 40, 50, 60, 128]);
    }

    #[test]
    fn cleanup_removes_only_old_pastes() {
        let dir = std::env::temp_dir().join(format!("deck-paste-clean-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = dir.join("paste-2.png");
        let old = dir.join("paste-1.png");
        let other = dir.join("keep.txt");
        std::fs::write(&fresh, b"x").unwrap();
        std::fs::write(&other, b"x").unwrap();
        std::fs::write(&old, b"x").unwrap();
        let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(two_days).unwrap();
        super::cleanup(&dir);
        assert!(fresh.exists() && other.exists(), "방금 만든 것·남의 파일은 남긴다");
        assert!(!old.exists(), "하루 넘은 붙여넣기는 치운다");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
