//! The demo as a real windowed app: it prints `STATE` lines so a session test can read
//! what a click or keystroke changed without a second channel.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::time::Duration;

use gpui_kit::assets::AllAssets;

fn main() {
    // A test hook: listen on the abstract socket `PILOT_DEMO_PLANT_ABSTRACT` names for the life
    // of the app, so the agent's post-launch isolation check has something to find. Abstract
    // names are per network namespace, so inside the offline sandbox it never reaches the host.
    let _planted = std::env::var("PILOT_DEMO_PLANT_ABSTRACT").ok().map(|name| {
        let address = SocketAddr::from_abstract_name(name.as_bytes()).expect("an abstract name");
        UnixListener::bind_addr(&address).expect("bind the planted socket")
    });
    gpui_kit::application().with_assets(AllAssets).run(|cx| {
        pilot_demo::init(cx);
        let (handle, demo) = pilot_demo::open(cx).expect("open pilot-demo");
        if std::env::var("PILOT_DEMO_GUARD_CLOSE").as_deref() == Ok("1") {
            handle
                .update(cx, |_, window, cx| {
                    pilot_demo::guard_close(demo.clone(), window, cx)
                })
                .expect("guard the demo's close");
        }
        let greeting = std::env::var("PILOT_DEMO_GREETING").ok();
        // Only the sandbox's private HOME is a safe place to keep the count, so it is opt-in.
        let persist = std::env::var("PILOT_DEMO_PERSIST").as_deref() == Ok("1");
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        demo.update(cx, |view, cx| {
            view.greeting = greeting;
            view.configure(&args);
            if let (true, Some(home)) = (persist, home) {
                view.persist_to(home.join("clicks"));
            }
            cx.notify();
        });

        // Report on the view and on its input, since typing notifies only the input.
        let name = demo.read(cx).name.clone();
        cx.observe(&demo, |view, cx| {
            println!("STATE {}", view.read(cx).state_line(cx))
        })
        .detach();
        cx.observe(&name, {
            let demo = demo.clone();
            move |_, cx| println!("STATE {}", demo.read(cx).state_line(cx))
        })
        .detach();
        println!("STATE opened");

        // A test hook: a `restart` or `quit` file in the working directory makes the app restart
        // itself or quit, so a session test can drive an exit the agent did not ask for.
        // Removing the file is the trigger, so a later poll never fires it twice.
        if std::env::var("PILOT_DEMO_RESTART").as_deref() == Ok("1") {
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    if std::fs::remove_file("restart").is_ok() {
                        println!("STATE restarting");
                        cx.update(|cx| cx.restart());
                    } else if std::fs::remove_file("quit").is_ok() {
                        println!("STATE quitting");
                        cx.update(|cx| cx.quit());
                    }
                }
            })
            .detach();
        }
    });
}
