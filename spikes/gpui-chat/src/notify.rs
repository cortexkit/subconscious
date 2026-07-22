use std::{collections::HashSet, process::Command, time::Duration};

use gpui::Context;

use crate::{app::SubcChat, models::AskRequest, wire};

#[derive(Default)]
pub(crate) struct AskNotificationState {
    known_ask_ids: Option<HashSet<String>>,
    in_flight: bool,
    last_error: Option<String>,
}

struct ArrivalNotice {
    title: String,
    body: String,
    critical: bool,
}

impl SubcChat {
    pub(crate) fn start_ask_notifications(&self, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor.timer(Duration::from_secs(5)).await;
                if this
                    .update(cx, |this, cx| this.poll_ask_notifications(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn poll_ask_notifications(&mut self, cx: &mut Context<Self>) {
        if !self.rooms.identity_ready || self.ask_notifications.in_flight {
            return;
        }
        self.ask_notifications.in_flight = true;
        let caller_directory = self.rooms.caller_directory.clone();
        let session_id = self.rooms.session_id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { wire::load_pending_asks_blocking(caller_directory, session_id) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.ask_notifications.in_flight = false;
                match result {
                    Ok(asks) => {
                        let notice =
                            arrival_notice(this.ask_notifications.known_ask_ids.as_ref(), &asks);
                        this.ask_notifications.known_ask_ids =
                            Some(asks.iter().map(|ask| ask.request_id.clone()).collect());
                        this.ask_notifications.last_error = None;
                        if this.snapshot.asks != asks {
                            this.snapshot.asks = asks;
                            this.selected_ask = this
                                .selected_ask
                                .min(this.snapshot.asks.len().saturating_sub(1));
                        }
                        update_dock_badge(this.snapshot.asks.len());
                        if let Some(notice) = notice {
                            request_attention_and_sound(notice.critical);
                            cx.background_executor()
                                .spawn(async move {
                                    if let Err(error) = post_banner(&notice.title, &notice.body) {
                                        eprintln!("post ask notification banner: {error}");
                                    }
                                })
                                .detach();
                        }
                    }
                    Err(error) => {
                        this.ask_notifications.last_error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

fn arrival_notice(known: Option<&HashSet<String>>, asks: &[AskRequest]) -> Option<ArrivalNotice> {
    let known = known?;
    let fresh: Vec<_> = asks
        .iter()
        .filter(|ask| !known.contains(&ask.request_id))
        .collect();
    if fresh.is_empty() {
        return None;
    }
    let critical = fresh.iter().any(|ask| {
        ask.urgency
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("high"))
            || ask.blocking == Some(true)
            || ask.material_damage == Some(true)
    });
    let title = match (fresh.len(), critical) {
        (1, true) => "Urgent ask from an agent".into(),
        (1, false) => "New ask from an agent".into(),
        (count, true) => format!("{count} new asks (urgent)"),
        (count, false) => format!("{count} new asks"),
    };
    let question = &fresh[0].question;
    let body = if question.chars().count() > 140 {
        format!("{}…", question.chars().take(140).collect::<String>())
    } else {
        question.clone()
    };
    Some(ArrivalNotice {
        title,
        body,
        critical,
    })
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
fn update_dock_badge(count: usize) {
    use cocoa::{
        appkit::NSApplication,
        base::{id, nil},
        foundation::NSString,
    };
    use objc::{msg_send, sel, sel_impl};

    // GPUI 0.2.2 exposes dock menus but not the dock tile badge, so this uses the
    // AppKit object GPUI already owns. This function is called only from the UI thread.
    unsafe {
        let app = NSApplication::sharedApplication(nil);
        let dock_tile: id = msg_send![app, dockTile];
        let label = if count == 0 {
            nil
        } else {
            NSString::alloc(nil).init_str(&count.to_string())
        };
        let _: () = msg_send![dock_tile, setBadgeLabel: label];
        if label != nil {
            let _: () = msg_send![label, release];
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn update_dock_badge(_: usize) {}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
fn request_attention_and_sound(critical: bool) {
    use cocoa::{
        appkit::{NSApplication, NSRequestUserAttentionType, NSSound},
        base::nil,
        foundation::NSString,
    };
    use objc::{sel, sel_impl};

    // AppKit requires these calls on the main thread; notification polling applies
    // results through GPUI's foreground context before reaching this function.
    unsafe {
        let app = NSApplication::sharedApplication(nil);
        app.requestUserAttention_(if critical {
            NSRequestUserAttentionType::NSCriticalRequest
        } else {
            NSRequestUserAttentionType::NSInformationalRequest
        });
        let name = NSString::alloc(nil).init_str(if critical { "Sosumi" } else { "Ping" });
        let sound = NSSound::soundNamed_(nil, name);
        if sound != nil {
            let _ = sound.play();
        }
        let _: () = objc::msg_send![name, release];
    }
}

#[cfg(not(target_os = "macos"))]
fn request_attention_and_sound(_: bool) {}

fn post_banner(title: &str, body: &str) -> std::io::Result<()> {
    let mut child = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            body,
        ])
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::arrival_notice;
    use crate::models::AskRequest;

    fn ask(id: &str, question: &str, urgent: bool) -> AskRequest {
        serde_json::from_value(serde_json::json!({
            "requestID": id,
            "question": question,
            "askedAt": 1,
            "urgency": if urgent { "high" } else { "normal" },
        }))
        .unwrap()
    }

    #[test]
    fn launch_backlog_is_quiet() {
        assert!(arrival_notice(None, &[ask("a", "old ask", true)]).is_none());
    }

    #[test]
    fn only_new_ids_are_announced_and_urgent_asks_escalate() {
        let known = HashSet::from(["old".to_string()]);
        let notice = arrival_notice(
            Some(&known),
            &[ask("old", "old ask", false), ask("new", "urgent ask", true)],
        )
        .unwrap();
        assert_eq!(notice.title, "Urgent ask from an agent");
        assert_eq!(notice.body, "urgent ask");
        assert!(notice.critical);
    }
}
