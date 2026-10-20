use super::*;

const QUICK: Duration = Duration::from_secs(2);

/// A stand-in `wl-paste`: `--list-types` prints `types`, `--type <t>` runs
/// `body` with `$3` set to the type asked for.
fn fake_wl_paste(dir: &std::path::Path, types: &str, body: &str) -> String {
    let script = format!(
        "if [ \"$1\" = --list-types ]; then printf '{types}'; exit 0; fi\n\
         [ \"$2\" = --type ] || exit 9\n{body}\n"
    );
    let path = dir.join("wl-paste.sh");
    std::fs::write(&path, script).unwrap();
    path.to_str().unwrap().to_owned()
}

fn paste(script: &str) -> Result<Clipboard, String> {
    read_with(ClipboardTool::WlPaste, &["sh", script], QUICK)
}

#[test]
fn an_offered_image_wins_over_offered_text() {
    let dir = tempfile::tempdir().unwrap();
    let body =
        "case \"$3\" in image/png) printf '\\211PNG\\r\\n\\032\\npixels';; *) printf words;; esac";
    let script = fake_wl_paste(dir.path(), "text/plain\\nimage/png\\n", body);
    let Clipboard::Image(image) = paste(&script).unwrap() else {
        panic!("the image is pasted, not the text");
    };
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.size_bytes(), 14);
}

#[test]
fn a_clipboard_with_only_text_pastes_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let body = "[ \"$3\" = 'text/plain;charset=utf-8' ] && printf 'hello there'";
    let types = "TEXT\\ntext/plain;charset=utf-8\\ntext/html\\n";
    let script = fake_wl_paste(dir.path(), types, body);
    assert_eq!(
        paste(&script).unwrap(),
        Clipboard::Text("hello there".into())
    );
}

#[test]
fn an_empty_clipboard_and_a_missing_helper_say_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("empty.sh");
    std::fs::write(&script, "echo 'Nothing is copied' >&2; exit 1\n").unwrap();
    let empty = paste(script.to_str().unwrap()).unwrap_err();
    assert_eq!(empty, NO_CONTENT);
    let only_html = fake_wl_paste(dir.path(), "text/html\\n", "printf '<b>x</b>'");
    assert_eq!(paste(&only_html).unwrap_err(), NO_CONTENT);
    let missing = read_with(ClipboardTool::Xclip, &["archon-no-such-helper"], QUICK);
    assert!(missing.unwrap_err().contains("install xclip"));
}

#[test]
fn an_oversized_image_is_refused_without_reading_all_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let body = "printf '\\211PNG\\r\\n\\032\\n'; exec cat /dev/zero";
    let script = fake_wl_paste(dir.path(), "image/png\\n", body);
    let refused = paste(&script).unwrap_err();
    assert!(refused.contains("too large"), "{refused}");
}

#[test]
fn a_helper_that_never_answers_is_given_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("stuck.sh");
    std::fs::write(&script, "exec sleep 30\n").unwrap();
    let started = Instant::now();
    let tool = ClipboardTool::WlPaste;
    let stuck = read_with(
        tool,
        &["sh", script.to_str().unwrap()],
        Duration::from_millis(200),
    );
    assert!(stuck.unwrap_err().contains("no answer within"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait is bounded"
    );
}

#[test]
fn the_session_decides_which_helper_reads_the_clipboard() {
    assert_eq!(
        clipboard_tool(false, true, true),
        Ok(ClipboardTool::WlPaste)
    );
    assert_eq!(clipboard_tool(false, false, true), Ok(ClipboardTool::Xclip));
    assert!(
        clipboard_tool(true, true, true)
            .unwrap_err()
            .contains("SSH")
    );
    assert!(
        clipboard_tool(false, false, false)
            .unwrap_err()
            .contains("/image")
    );
    let xclip = ClipboardTool::Xclip.read_args("image/png");
    assert_eq!(xclip, ["-selection", "clipboard", "-o", "-t", "image/png"]);
}
