mod common;

use std::path::{Path, PathBuf};

use common::with_platform;
use gpui_kit::{ClipboardItem, PathPromptOptions, Platform, SystemNotification, WindowingRequest};
use gpui_pilot_host::{PathPrompt, PromptAnswered, Recorded};

#[test]
fn path_prompts_wait_for_an_answer_take_a_queued_one_or_cancel() {
    let results = with_platform(|platform, cx| async move {
        let options = PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        };
        let open = cx.update(|cx| cx.prompt_for_paths(options.clone()));
        let pending = platform.pending_prompt();
        let answered = platform.answer_prompt(Some(vec!["/a".into(), "/b".into()]));
        let opened = open.await.unwrap().unwrap();
        let queued = platform.answer_prompt(Some(vec!["/s".into(), "/ignored".into()]));
        let save = cx.update(|cx| cx.prompt_for_new_path(Path::new("/tmp"), Some("s.txt")));
        let saved = save.await.unwrap().unwrap();
        let cancel = cx.update(|cx| cx.prompt_for_paths(options));
        let cancelled = platform.answer_prompt(None);
        let none = cancel.await.unwrap().unwrap();
        (pending, answered, opened, queued, saved, cancelled.0, none)
    });
    let open = PathPrompt::Open {
        directories: false,
        multiple: true,
    };
    assert_eq!(results.0, Some(open.clone()));
    assert_eq!(results.1, (PromptAnswered::Open, Some(open)));
    assert_eq!(results.2, Some(vec![PathBuf::from("/a"), "/b".into()]));
    assert_eq!(results.3, (PromptAnswered::Queued, None));
    assert_eq!(results.4, Some(PathBuf::from("/s")));
    assert_eq!((results.5, results.6), (PromptAnswered::Cancelled, None));
}

#[test]
fn clipboard_and_credentials_stay_in_memory() {
    let (read, written, stored, deleted) = with_platform(|platform, cx| async move {
        platform.set_clipboard_text("from agent".into());
        let read = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("from app".into())));
        let url = "https://api.invalid";
        cx.update(|cx| cx.write_credentials(url, "user", b"secret"))
            .await
            .unwrap();
        let stored = cx.update(|cx| cx.read_credentials(url)).await.unwrap();
        cx.update(|cx| cx.delete_credentials(url)).await.unwrap();
        let deleted = cx.update(|cx| cx.read_credentials(url)).await.unwrap();
        (read, platform.clipboard_text(), stored, deleted)
    });
    assert_eq!(read.as_deref(), Some("from agent"));
    assert_eq!(written.as_deref(), Some("from app"));
    assert_eq!(stored, Some(("user".to_string(), b"secret".to_vec())));
    assert_eq!(deleted, None);
}

#[test]
fn side_effects_are_recorded_not_run() {
    let (recorded, refused) = with_platform(|platform, cx| async move {
        cx.update(|cx| {
            cx.open_url("https://example.invalid/");
            cx.open_with_system(Path::new("/tmp/pilot-x"));
            cx.reveal_path(Path::new("/tmp/pilot-y"));
        });
        Platform::restart(&*platform, None, Vec::new());
        let note = SystemNotification {
            tag: "t".into(),
            title: "Hello".into(),
            body: "b".into(),
            actions: Vec::new(),
        };
        Platform::show_system_notification(&*platform, note);
        let sleep = Platform::prevent_idle_sleep(&*platform, "test").await;
        let windowing = Platform::request_windowing(&*platform, WindowingRequest::Headless).await;
        (platform.recorded(), sleep.is_err() && windowing.is_err())
    });
    assert_eq!(
        recorded,
        vec![
            Recorded::OpenUrl("https://example.invalid/".into()),
            Recorded::OpenWithSystem("/tmp/pilot-x".into()),
            Recorded::RevealPath("/tmp/pilot-y".into()),
            Recorded::Restart(None),
            Recorded::Notification {
                title: "Hello".into()
            },
        ]
    );
    assert!(refused, "idle inhibition and windowing changes are refused");
}
