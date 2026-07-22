mod app;
mod asks;
mod boards;
mod chat;
mod components;
mod input;
mod models;
mod notify;
mod observe;
mod rooms;
mod wire;

use app::SubcChat;
use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};

fn main() {
    if std::env::args().any(|arg| arg == "--probe-rooms") {
        match wire::probe_rooms_blocking() {
            Ok(count) => println!("rooms: {count} visible rooms"),
            Err(error) => {
                eprintln!("rooms probe failed: {error:#}");
                std::process::exit(2);
            }
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--probe-chat") {
        match wire::probe_chat_blocking() {
            Ok(message) => println!("chat: {message}"),
            Err(error) => {
                eprintln!("chat probe failed: {error:#}");
                std::process::exit(2);
            }
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--probe-observe") {
        match wire::probe_observe_blocking() {
            Ok(summary) => println!("observe: {summary}"),
            Err(error) => {
                eprintln!("observe probe failed: {error:#}");
                std::process::exit(2);
            }
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--probe-live") {
        match wire::load_live_blocking() {
            Ok(snapshot) => println!(
                "live: {} boards, {} asks, {} consults, {} campaigns",
                snapshot.boards.len(),
                snapshot.asks.len(),
                snapshot.consults.len(),
                snapshot.campaigns.len()
            ),
            Err(error) => {
                eprintln!("live probe failed: {error:#}");
                std::process::exit(2);
            }
        }
        return;
    }
    Application::new().run(|cx: &mut App| {
        input::bind_keys(cx);
        chat::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(980.), px(640.))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("SubcChat — GPUI".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(SubcChat::new),
        )
        .expect("open GPUI window");
        cx.activate(true);
    });
}
